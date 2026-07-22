//! Embedded `pagh-mini` Rust toolchain.
//!
//! This is intentionally a small, offline Rust-like language rather than the
//! upstream rustc compiler. It makes source written in nano directly runnable
//! and provides familiar `cargo`, `rustc`, `rust`, and `rustup` workflows.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use crate::vfs::{VfsError, VfsNode};

const MAGIC: &str = "PAGH-MINI-RUST:1\n";
const MAX_SOURCE: usize = 64 * 1024;

fn out(s: &str) { crate::kprintln!("{}", s); crate::fb_println!("{}", s); }
fn err(s: &str) { super::render::error_line(s); }

#[derive(Clone, Debug)]
enum Value { Int(i64), List(Vec<i64>) }
impl Value { fn int(&self) -> Result<i64,String> { match self {Value::Int(v)=>Ok(*v),_=>Err("expected integer".into())} } }

type Env = BTreeMap<String, Value>;

struct Expr<'a> { b: &'a [u8], p: usize, env: &'a Env }
impl<'a> Expr<'a> {
    fn new(s:&'a str,env:&'a Env)->Self{Self{b:s.as_bytes(),p:0,env}}
    fn ws(&mut self){while self.p<self.b.len()&&self.b[self.p].is_ascii_whitespace(){self.p+=1}}
    fn eat(&mut self,c:u8)->bool{self.ws();if self.p<self.b.len()&&self.b[self.p]==c{self.p+=1;true}else{false}}
    fn parse(mut self)->Result<Value,String>{let v=self.add()?;self.ws();if self.p!=self.b.len(){Err(format!("unexpected expression near '{}'",String::from_utf8_lossy(&self.b[self.p..])))}else{Ok(Value::Int(v))}}
    fn add(&mut self)->Result<i64,String>{let mut v=self.mul()?;loop{if self.eat(b'+'){v=v.checked_add(self.mul()?).ok_or("integer overflow")?}else if self.eat(b'-'){v=v.checked_sub(self.mul()?).ok_or("integer overflow")?}else{return Ok(v)}}}
    fn mul(&mut self)->Result<i64,String>{let mut v=self.atom()?;loop{if self.eat(b'*'){v=v.checked_mul(self.atom()?).ok_or("integer overflow")?}else if self.eat(b'/'){let d=self.atom()?;if d==0{return Err("division by zero".into())}v/=d}else if self.eat(b'%'){let d=self.atom()?;if d==0{return Err("division by zero".into())}v%=d}else{return Ok(v)}}}
    fn atom(&mut self)->Result<i64,String>{self.ws();if self.eat(b'('){let v=self.add()?;if !self.eat(b')'){return Err("missing ')'".into())}return Ok(v)}
        let neg=self.eat(b'-');self.ws();let start=self.p;while self.p<self.b.len()&&self.b[self.p].is_ascii_digit(){self.p+=1}if self.p>start{let s=core::str::from_utf8(&self.b[start..self.p]).map_err(|_|"bad number")?;let mut v=s.parse::<i64>().map_err(|_|"bad number")?;if neg{v=-v}return Ok(v)}
        let start=self.p;while self.p<self.b.len()&&(self.b[self.p].is_ascii_alphanumeric()||self.b[self.p]==b'_'){self.p+=1}if self.p==start{return Err("expected value".into())}let name=core::str::from_utf8(&self.b[start..self.p]).map_err(|_|"bad identifier")?;
        if self.b.get(self.p..).map_or(false,|r|r.starts_with(b".iter().sum()")){self.p+=13;return match self.env.get(name){Some(Value::List(v))=>Ok(v.iter().copied().sum()),_=>Err(format!("{} is not a list",name))}}
        self.env.get(name).ok_or_else(||format!("unknown variable '{}'",name))?.int()
    }
}

fn eval(s:&str,env:&Env)->Result<Value,String>{let t=s.trim();if t.starts_with('[')&&t.ends_with(']'){let mut v=Vec::new();for part in t[1..t.len()-1].split(','){if !part.trim().is_empty(){v.push(Expr::new(part,env).parse()?.int()?);}}Ok(Value::List(v))}else{Expr::new(t,env).parse()}}

fn string_literal(s:&str)->Result<String,String>{let t=s.trim();if !t.starts_with('"')||!t.ends_with('"'){return Err("expected string literal".into())}let mut out=String::new();let mut esc=false;for c in t[1..t.len()-1].chars(){if esc{out.push(match c{'n'=>'\n','t'=>'\t','r'=>'\r','"'=>'"','\\'=>'\\',x=>x});esc=false}else if c=='\\'{esc=true}else{out.push(c)}}Ok(out)}

fn split_args(s:&str)->Vec<&str>{let mut out=Vec::new();let mut start=0;let mut quote=false;let mut depth=0i32;let b=s.as_bytes();for(i,&c)in b.iter().enumerate(){match c{b'"'=>quote=!quote,b'('|b'[' if !quote=>depth+=1,b')'|b']' if !quote=>depth-=1,b',' if !quote&&depth==0=>{out.push(s[start..i].trim());start=i+1},_=>{}}}out.push(s[start..].trim());out}

fn render_format(fmt:&str,args:&[Value])->String{let mut result=String::new();let mut rest=fmt;let mut i=0;while let Some(p)=rest.find("{}") {result.push_str(&rest[..p]);if let Some(v)=args.get(i){match v{Value::Int(n)=>result.push_str(&n.to_string()),Value::List(a)=>result.push_str(&format!("{:?}",a))}}else{result.push_str("{}")};i+=1;rest=&rest[p+2..];}result.push_str(rest);result}

fn brace_delta(s:&str)->i32{s.bytes().map(|b|if b==b'{'{1}else if b==b'}'{-1}else{0}).sum()}
fn matching(lines:&[&str],start:usize)->Result<usize,String>{let mut d=0;for i in start..lines.len(){d+=brace_delta(lines[i]);if d==0&&i>start{return Ok(i)}}Err("unclosed block".into())}

fn execute_range(lines:&[&str],start:usize,end:usize,env:&mut Env)->Result<(),String>{let mut i=start;while i<end{let raw=lines[i];let line=raw.split("//").next().unwrap_or("").trim();if line.is_empty()||line=="{"||line=="}"||line.starts_with("use "){i+=1;continue}
        if line.starts_with("for "){let open=line.find('{').ok_or_else(||format!("line {}: for needs '{{'",i+1))?;let head=&line[4..open].trim();let (name,range)=head.split_once(" in ").ok_or_else(||format!("line {}: bad for",i+1))?;let inclusive=range.contains("..=");let parts:Vec<&str>=range.split(if inclusive{"..="}else{".."}).collect();if parts.len()!=2{return Err(format!("line {}: bad range",i+1))}let a=eval(parts[0],env)?.int()?;let b=eval(parts[1],env)?.int()?;let close=matching(lines,i)?;let stop=if inclusive{b.saturating_add(1)}else{b};for n in a..stop{env.insert(name.trim().to_string(),Value::Int(n));execute_range(lines,i+1,close,env)?;}i=close+1;continue}
        if line.starts_with("println!")||line.starts_with("print!"){let newline=line.starts_with("println!");let l=line.find('(').ok_or("bad print")?;let r=line.rfind(')').ok_or("bad print")?;let args=split_args(&line[l+1..r]);let fmt=string_literal(args.first().copied().unwrap_or(""))?;let mut vals=Vec::new();for a in args.iter().skip(1){vals.push(eval(a,env)?)}let text=render_format(&fmt,&vals);if newline{out(&text)}else{crate::kprint!("{}",text);crate::fb_print!("{}",text)}i+=1;continue}
        if line.starts_with("let "){let body=line.trim_end_matches(';')[4..].trim();let (lhs,rhs)=body.split_once('=').ok_or_else(||format!("line {}: let needs '='",i+1))?;let mut name=lhs.trim().trim_start_matches("mut ").trim();if let Some((n,_))=name.split_once(':'){name=n.trim()}if name.is_empty(){return Err(format!("line {}: missing name",i+1))}env.insert(name.to_string(),eval(rhs,env)?);i+=1;continue}
        if let Some((lhs,rhs))=line.trim_end_matches(';').split_once('='){let name=lhs.trim();if env.contains_key(name){let v=eval(rhs,env)?;env.insert(name.to_string(),v);i+=1;continue}}
        if line.starts_with("fn ")||line.starts_with("#![")||line.starts_with("#["){i+=1;continue}
        return Err(format!("line {}: unsupported syntax: {}",i+1,line));}
    Ok(())}

pub fn execute(source:&str)->Result<(),String>{let lines:Vec<&str>=source.lines().collect();let main=lines.iter().position(|l|l.trim_start().starts_with("fn main" )).ok_or("missing fn main()")?;if !lines[main].contains('{'){return Err("fn main must open with '{'".into())}let close=matching(&lines,main)?;let mut env=Env::new();execute_range(&lines,main+1,close,&mut env)}

fn read_file(path:&str)->Result<Vec<u8>,VfsError>{let n=crate::vfs::lookup_path(path)?;if n.is_directory(){return Err(VfsError::InvalidArgument)}let size=(n.size() as usize).min(MAX_SOURCE+MAGIC.len());let mut b=alloc::vec![0;size];let got=n.read(0,&mut b)?;b.truncate(got);Ok(b)}
fn parent_leaf(path:&str)->Result<(&str,&str),VfsError>{let t=path.trim_end_matches('/');let i=t.rfind('/').ok_or(VfsError::InvalidArgument)?;let leaf=&t[i+1..];if leaf.is_empty(){return Err(VfsError::InvalidArgument)}Ok((if i==0{"/"}else{&t[..i]},leaf))}
fn mkdir_all(path:&str)->Result<Arc<dyn VfsNode>,VfsError>{let mut cur=crate::vfs::lookup_path("/")?;for c in path.split('/').filter(|x|!x.is_empty()){cur=match cur.lookup(c){Ok(n)=>n,Err(VfsError::NotFound)=>cur.create_dir(c)?,Err(e)=>return Err(e)};if !cur.is_directory(){return Err(VfsError::InvalidArgument)}}Ok(cur)}
fn write_file(path:&str,data:&[u8])->Result<(),VfsError>{let (parent,leaf)=parent_leaf(path)?;let dir=mkdir_all(parent)?;let node=match dir.lookup(leaf){Ok(n)=>n,Err(VfsError::NotFound)=>dir.create_file(leaf)?,Err(e)=>return Err(e)};node.truncate(0)?;if !data.is_empty(){let n=node.write(0,data)?;if n!=data.len(){return Err(VfsError::IoError)}}node.sync();Ok(())}
fn source_from(path:&str)->Result<String,String>{let b=read_file(path).map_err(|e|format!("{:?}",e))?;let body=if b.starts_with(MAGIC.as_bytes()){&b[MAGIC.len()..]}else{&b};if body.len()>MAX_SOURCE{return Err("source exceeds 64 KiB".into())}core::str::from_utf8(body).map(|s|s.to_string()).map_err(|_|"source is not UTF-8".into())}
fn compile(source:&str)->Result<Vec<u8>,String>{if !source.contains("fn main"){return Err("missing fn main()".into())}let mut out=MAGIC.as_bytes().to_vec();out.extend_from_slice(source.as_bytes());Ok(out)}

pub fn run_path(path_arg:&str)->Result<(),String>{let path=super::path::resolve(&super::path::cwd(),path_arg);execute(&source_from(&path)?) }

pub fn rustc(args:&[&str]){if args.is_empty(){out("usage: rustc <file.rs> [-o output.pbc]");return}let src=super::path::resolve(&super::path::cwd(),args[0]);let out_path=if let Some(i)=args.iter().position(|x|*x=="-o"){args.get(i+1).map(|x|super::path::resolve(&super::path::cwd(),x)).unwrap_or_else(||format!("{}.pbc",src))}else{format!("{}.pbc",src.trim_end_matches(".rs"))};match source_from(&src).and_then(|s|compile(&s)){Ok(bytes)=>match write_file(&out_path,&bytes){Ok(())=>out(&format!("rustc: built {}",out_path)),Err(e)=>err(&format!("rustc: write failed: {:?}",e))},Err(e)=>err(&format!("rustc: {}",e))}}

fn package_name(root:&str)->String{let p=format!("{}/Cargo.toml",root.trim_end_matches('/'));if let Ok(b)=read_file(&p){if let Ok(s)=core::str::from_utf8(&b){for l in s.lines(){let t=l.trim();if t.starts_with("name") {if let Some((_,v))=t.split_once('='){return v.trim().trim_matches('"').to_string()}}}}}root.trim_end_matches('/').rsplit('/').next().unwrap_or("app").to_string()}
fn build_project(root:&str)->Result<String,String>{let src=format!("{}/src/main.rs",root.trim_end_matches('/'));let source=source_from(&src)?;let bytes=compile(&source)?;let target=format!("{}/target/debug/{}.pbc",root.trim_end_matches('/'),package_name(root));write_file(&target,&bytes).map_err(|e|format!("write failed: {:?}",e))?;Ok(target)}

pub fn cargo(args:&[&str]){if args.is_empty(){out("cargo 0.1.0 (pagh-mini)");out("commands: new, check, build, run");return}match args[0]{
    "new"=>{let Some(p)=args.get(1)else{err("cargo new <path>");return};let root=super::path::resolve(&super::path::cwd(),p);let name=root.trim_end_matches('/').rsplit('/').next().unwrap_or("app");if let Err(e)=mkdir_all(&format!("{}/src",root)).and_then(|_|write_file(&format!("{}/Cargo.toml",root),format!("[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",name).as_bytes())).and_then(|_|write_file(&format!("{}/src/main.rs",root),b"fn main() {\n    println!(\"Hello from pagh mini-Rust!\");\n}\n")){err(&format!("cargo new: {:?}",e))}else{out(&format!("Created binary package '{}'",root))}},
    "check"|"build"=>{let root=args.get(1).map(|p|super::path::resolve(&super::path::cwd(),p)).unwrap_or_else(super::path::cwd);match build_project(&root){Ok(t)=>out(&format!("Finished dev target: {}",t)),Err(e)=>err(&format!("cargo: {}",e))}},
    "run"=>{let root=args.get(1).map(|p|super::path::resolve(&super::path::cwd(),p)).unwrap_or_else(super::path::cwd);match build_project(&root){Ok(t)=>{out(&format!("Running {}",t));if let Err(e)=run_path(&t){err(&format!("runtime error: {}",e))}},Err(e)=>err(&format!("cargo: {}",e))}},
    "--version"|"-V"=>out("cargo 0.1.0 (pagh-mini)"),_=>err("cargo: supported commands are new, check, build, run")}}

pub fn rustup(args:&[&str]){match args.first().copied(){None|Some("show")=>{out("Default host: x86_64-pagh");out("active toolchain: pagh-mini 0.1.0 (embedded)");out("installed targets: x86_64-pagh")},Some("target") if args.get(1)==Some(&"list")=>out("x86_64-pagh (installed)"),Some("default")=>out("pagh-mini is the built-in default; network toolchain downloads are unavailable"),Some("--version")=>out("rustup 0.1.0 (pagh-mini offline)"),_=>err("rustup: supported commands: show, target list, default")}}
