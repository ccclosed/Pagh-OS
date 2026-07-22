//! Persistent nano+ configuration stored in `/mnt/.nanorc`.

use alloc::format;
use alloc::string::{String, ToString};
use crate::vfs::{VfsError, VfsNode};

pub const CONFIG_PATH: &str = "/mnt/.nanorc";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Theme { Dark, Light, Blue }

#[derive(Clone)]
pub struct NanoConfig {
    pub tab_size: usize,
    pub auto_indent: bool,
    pub line_numbers: bool,
    pub trim_trailing: bool,
    pub backup: bool,
    pub show_whitespace: bool,
    pub wrap_column: usize,
    pub theme: Theme,
}

impl Default for NanoConfig {
    fn default() -> Self {
        Self { tab_size: 4, auto_indent: true, line_numbers: true,
            trim_trailing: false, backup: true, show_whitespace: false,
            wrap_column: 0, theme: Theme::Dark }
    }
}

impl NanoConfig {
    pub fn load() -> Self {
        let mut cfg=Self::default();
        let Ok(node)=crate::vfs::lookup_path(CONFIG_PATH) else{return cfg};
        let size=(node.size() as usize).min(8192); let mut buf=alloc::vec![0u8;size];
        let Ok(n)=node.read(0,&mut buf) else{return cfg};
        if let Ok(text)=core::str::from_utf8(&buf[..n]) {
            for raw in text.lines() {
                let line=raw.split('#').next().unwrap_or("").trim();
                let Some((key,value))=line.split_once('=') else{continue};
                let key=key.trim(); let value=value.trim();
                match key {
                    "tab_size" => if let Ok(n)=value.parse::<usize>() {cfg.tab_size=n.clamp(1,8)},
                    "auto_indent" => if let Some(v)=parse_bool(value){cfg.auto_indent=v},
                    "line_numbers" => if let Some(v)=parse_bool(value){cfg.line_numbers=v},
                    "trim_trailing" => if let Some(v)=parse_bool(value){cfg.trim_trailing=v},
                    "backup" => if let Some(v)=parse_bool(value){cfg.backup=v},
                    "show_whitespace" => if let Some(v)=parse_bool(value){cfg.show_whitespace=v},
                    "wrap_column" => if let Ok(n)=value.parse::<usize>() {cfg.wrap_column=n.min(240)},
                    "theme" => if let Some(v)=parse_theme(value){cfg.theme=v},
                    _=>{}
                }
            }
        }
        cfg
    }
    pub fn encode(&self)->String { format!(
        "# pagh nano+ settings\ntab_size={}\nauto_indent={}\nline_numbers={}\ntrim_trailing={}\nbackup={}\nshow_whitespace={}\nwrap_column={}\ntheme={}\n",
        self.tab_size,self.auto_indent,self.line_numbers,self.trim_trailing,self.backup,
        self.show_whitespace,self.wrap_column,theme_name(self.theme)) }
    pub fn palette(&self)->[u32;8] { match self.theme {
        Theme::Dark=>[0x191919,0x252525,0xF2F2F2,0x9B9B9B,0x2783DE,0x46A171,0xE56458,0x24496D],
        Theme::Light=>[0xF5F5F5,0xE0E0E0,0x151515,0x666666,0x1769AA,0x247A4A,0xC62828,0x90CAF9],
        Theme::Blue=>[0x071A2B,0x0D2942,0xE6F4FF,0x88AFC9,0x1689D9,0x4FC38A,0xFF6B6B,0x174D73],
    }}
}

fn parse_bool(v:&str)->Option<bool>{match v{"true"|"on"|"yes"|"1"=>Some(true),"false"|"off"|"no"|"0"=>Some(false),_=>None}}
fn parse_theme(v:&str)->Option<Theme>{match v{"dark"=>Some(Theme::Dark),"light"=>Some(Theme::Light),"blue"=>Some(Theme::Blue),_=>None}}
fn theme_name(v:Theme)->&'static str{match v{Theme::Dark=>"dark",Theme::Light=>"light",Theme::Blue=>"blue"}}
fn out(s:&str){crate::kprintln!("{}",s);crate::fb_println!("{}",s)}

fn save(cfg:&NanoConfig)->Result<(),VfsError>{
    let root=crate::vfs::lookup_path("/mnt")?;
    let node=match root.lookup(".nanorc"){Ok(n)=>n,Err(VfsError::NotFound)=>root.create_file(".nanorc")?,Err(e)=>return Err(e)};
    let text=cfg.encode(); node.truncate(0)?;
    if node.write(0,text.as_bytes())?!=text.len(){return Err(VfsError::IoError)} node.sync();Ok(())
}

pub fn command(args:&[&str]) {
    let mut cfg=NanoConfig::load();
    if args.is_empty() || args[0]=="show" {
        out(&format!("nano config: {}",CONFIG_PATH));
        for line in cfg.encode().lines().filter(|l|!l.starts_with('#')){out(line)}
        out("usage: nano --settings set <key> <value> | reset"); return;
    }
    if args[0]=="reset" {cfg=NanoConfig::default();}
    else if args[0]=="set" && args.len()>=3 {
        let value=args[2]; let ok=match args[1] {
            "tab_size"=>value.parse::<usize>().ok().filter(|n|(1..=8).contains(n)).map(|n|cfg.tab_size=n).is_some(),
            "auto_indent"=>parse_bool(value).map(|v|cfg.auto_indent=v).is_some(),
            "line_numbers"=>parse_bool(value).map(|v|cfg.line_numbers=v).is_some(),
            "trim_trailing"=>parse_bool(value).map(|v|cfg.trim_trailing=v).is_some(),
            "backup"=>parse_bool(value).map(|v|cfg.backup=v).is_some(),
            "show_whitespace"=>parse_bool(value).map(|v|cfg.show_whitespace=v).is_some(),
            "wrap_column"=>value.parse::<usize>().ok().filter(|n|*n<=240).map(|n|cfg.wrap_column=n).is_some(),
            "theme"=>parse_theme(value).map(|v|cfg.theme=v).is_some(), _=>false};
        if !ok{out("nano: invalid setting or value");return}
    } else {out("usage: nano --settings [show|reset|set <key> <value>]");return}
    match save(&cfg){Ok(())=>out("nano: settings saved"),Err(e)=>out(&format!("nano: cannot save settings: {:?}",e))}
}
