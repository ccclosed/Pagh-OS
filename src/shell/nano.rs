//! `nano+` — a small full-screen text editor for the framebuffer shell.
//!
//! Compared with classic nano it adds line numbers, a persistent status bar,
//! bounded undo/redo, incremental search, go-to-line, horizontal scrolling and
//! a dirty-file quit guard. Input remains ASCII because the PS/2 keyboard map is
//! ASCII; existing UTF-8 bytes are displayed lossily rather than split unsafely.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::drivers::{cursor, framebuffer};
use crate::vfs::{VfsError, VfsNode};
use super::keys::{Decoder, KeyEvent};
use super::nano_config::NanoConfig;

const MAX_FILE: usize = 64 * 1024;
const MAX_LINE: usize = 4096;
const UNDO_LIMIT: usize = 32;
const CHAR_W: usize = 8;
const CHAR_H: usize = 16;
const HEADER_H: usize = 22;
const FOOTER_H: usize = 38;
const BG: u32 = 0x191919;
const SURFACE: u32 = 0x252525;
const TEXT: u32 = 0xF2F2F2;
const MUTED: u32 = 0x9B9B9B;
const BLUE: u32 = 0x2783DE;
const GREEN: u32 = 0x46A171;
const RED: u32 = 0xE56458;
const SELECT: u32 = 0x24496D;

#[derive(Clone)]
struct Snapshot {
    lines: Vec<String>,
    row: usize,
    col: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PromptMode { Search, Goto, ReplaceFind, ReplaceWith }

pub struct Editor {
    path: String,
    lines: Vec<String>,
    row: usize,
    col: usize,
    top: usize,
    left: usize,
    dirty: bool,
    status: String,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    prompt: Option<(PromptMode, String)>,
    search_hit: Option<(usize, usize, usize)>,
    quit_armed: bool,
    config: NanoConfig,
    clipboard: Vec<String>,
    replace_needle: Option<String>,
}

impl Editor {
    fn new(path: &str, text: &str, config: NanoConfig) -> Self {
        let mut lines: Vec<String> = text.split('\n').map(sanitize_line).collect();
        if lines.is_empty() { lines.push(String::new()); }
        Self {
            path: path.to_string(), lines, row: 0, col: 0, top: 0, left: 0,
            dirty: false, status: "Ready".to_string(), undo: Vec::new(), redo: Vec::new(),
            prompt: None, search_hit: None, quit_armed: false, config,
            clipboard: Vec::new(), replace_needle: None,
        }
    }

    fn snapshot(&self) -> Snapshot { Snapshot { lines: self.lines.clone(), row: self.row, col: self.col } }
    fn checkpoint(&mut self) {
        if self.undo.len() == UNDO_LIMIT { self.undo.remove(0); }
        self.undo.push(self.snapshot());
        self.redo.clear(); self.dirty = true; self.quit_armed = false;
    }
    fn restore(&mut self, s: Snapshot) {
        self.lines = s.lines; self.row = s.row.min(self.lines.len().saturating_sub(1));
        self.col = s.col.min(self.lines[self.row].len()); self.ensure_visible();
    }
    fn undo(&mut self) {
        if let Some(s) = self.undo.pop() { let cur=self.snapshot(); self.redo.push(cur); self.restore(s); self.dirty=true; self.status="Undo".into(); }
        else { self.status="Nothing to undo".into(); }
    }
    fn redo(&mut self) {
        if let Some(s) = self.redo.pop() { let cur=self.snapshot(); self.undo.push(cur); self.restore(s); self.dirty=true; self.status="Redo".into(); }
        else { self.status="Nothing to redo".into(); }
    }
    fn insert(&mut self, ch: char) {
        if self.lines[self.row].len() >= MAX_LINE { self.status="Line length limit reached".into(); return; }
        self.checkpoint(); self.lines[self.row].insert(self.col, ch); self.col += ch.len_utf8();
        if self.config.wrap_column > 0 && self.col >= self.config.wrap_column { self.newline_without_checkpoint(); }
        self.ensure_visible();
    }
    fn newline_without_checkpoint(&mut self) {
        let indent = if self.config.auto_indent {
            self.lines[self.row].bytes().take_while(|b| *b == b' ').count()
        } else { 0 };
        let tail=self.lines[self.row].split_off(self.col); self.row+=1;
        let mut next=String::new(); for _ in 0..indent {next.push(' ')} next.push_str(&tail);
        self.lines.insert(self.row,next); self.col=indent; self.ensure_visible();
    }
    fn newline(&mut self) { self.checkpoint(); self.newline_without_checkpoint(); }
    fn backspace(&mut self) {
        if self.col>0 { self.checkpoint(); self.col-=1; self.lines[self.row].remove(self.col); }
        else if self.row>0 { self.checkpoint(); let current=self.lines.remove(self.row); self.row-=1; self.col=self.lines[self.row].len(); if self.col+current.len()<=MAX_LINE { self.lines[self.row].push_str(&current); } else { self.lines.insert(self.row+1,current); self.row+=1; self.col=0; } }
        self.ensure_visible();
    }
    fn delete(&mut self) {
        if self.col<self.lines[self.row].len() { self.checkpoint(); self.lines[self.row].remove(self.col); }
        else if self.row+1<self.lines.len() && self.lines[self.row].len()+self.lines[self.row+1].len()<=MAX_LINE { self.checkpoint(); let next=self.lines.remove(self.row+1); self.lines[self.row].push_str(&next); }
    }
    fn left(&mut self) { if self.col>0 {self.col-=1} else if self.row>0 {self.row-=1;self.col=self.lines[self.row].len()} self.ensure_visible(); }
    fn right(&mut self) { if self.col<self.lines[self.row].len(){self.col+=1}else if self.row+1<self.lines.len(){self.row+=1;self.col=0} self.ensure_visible(); }
    fn up(&mut self) { if self.row>0 {self.row-=1;self.col=self.col.min(self.lines[self.row].len())} self.ensure_visible(); }
    fn down(&mut self) { if self.row+1<self.lines.len(){self.row+=1;self.col=self.col.min(self.lines[self.row].len())} self.ensure_visible(); }
    fn copy_line(&mut self) { self.clipboard=alloc::vec![self.lines[self.row].clone()]; self.status="Line copied".into(); }
    fn cut_line(&mut self) { self.checkpoint(); self.clipboard=alloc::vec![self.lines.remove(self.row)]; if self.lines.is_empty(){self.lines.push(String::new())} self.row=self.row.min(self.lines.len()-1); self.col=self.col.min(self.lines[self.row].len()); self.status="Line cut".into(); self.ensure_visible(); }
    fn paste_line(&mut self) { if self.clipboard.is_empty(){self.status="Clipboard empty".into();return} self.checkpoint(); let at=self.row+1; for (i,line) in self.clipboard.clone().into_iter().enumerate(){self.lines.insert(at+i,line)} self.row=at;self.col=0;self.status="Pasted".into();self.ensure_visible(); }
    fn ensure_visible(&mut self) {
        let (w,h)=framebuffer::dimensions(); let rows=h.saturating_sub(HEADER_H+FOOTER_H)/CHAR_H;
        let cols=w/CHAR_W; let gutter=if self.config.line_numbers {7}else{0};
        if self.row<self.top {self.top=self.row} else if rows>0 && self.row>=self.top+rows {self.top=self.row+1-rows}
        if self.col<self.left {self.left=self.col} else if cols>gutter && self.col>=self.left+cols-gutter {self.left=self.col+1-(cols-gutter)}
    }
    fn begin_prompt(&mut self, mode: PromptMode) { self.prompt=Some((mode,String::new())); self.status=match mode {PromptMode::Search=>"Search",PromptMode::Goto=>"Go to line",PromptMode::ReplaceFind=>"Replace: find",PromptMode::ReplaceWith=>"Replace with"}.into(); }
    fn commit_prompt(&mut self) {
        let Some((mode,value))=self.prompt.take() else{return};
        match mode {
            PromptMode::Search => self.find_next(&value),
            PromptMode::Goto => match value.parse::<usize>() { Ok(n) if n>0 => {self.row=(n-1).min(self.lines.len()-1);self.col=self.col.min(self.lines[self.row].len());self.status=format!("Line {}",self.row+1);self.ensure_visible();}, _=>self.status="Invalid line number".into() },
            PromptMode::ReplaceFind => { if value.is_empty(){self.status="Empty search".into()}else{self.replace_needle=Some(value);self.begin_prompt(PromptMode::ReplaceWith)} },
            PromptMode::ReplaceWith => { let Some(needle)=self.replace_needle.take() else{return}; let count:usize=self.lines.iter().map(|l|l.matches(&needle).count()).sum(); if count>0{self.checkpoint();for line in &mut self.lines{*line=line.replace(&needle,&value)}} self.status=format!("Replaced {} occurrence(s)",count); }
        }
    }
    fn find_next(&mut self, needle:&str) {
        if needle.is_empty(){self.status="Empty search".into();return}
        let start_row=self.row; let start_col=(self.col+1).min(self.lines[self.row].len());
        for pass in 0..2 { let from=if pass==0{start_row}else{0}; let to=if pass==0{self.lines.len()}else{start_row+1};
            for r in from..to { let off=if pass==0&&r==start_row{start_col}else{0}; if let Some(i)=self.lines[r][off..].find(needle){let c=off+i;self.row=r;self.col=c;self.search_hit=Some((r,c,needle.len()));self.status=format!("Found '{}'",needle);self.ensure_visible();return} }
        }
        self.status=format!("'{}' not found",needle);
    }
    fn handle_prompt(&mut self,event:KeyEvent) {
        match event {
            KeyEvent::Char(c) => if let Some((_,s))=&mut self.prompt { if s.len()<64{s.push(c)} },
            KeyEvent::Backspace => if let Some((_,s))=&mut self.prompt {s.pop();},
            KeyEvent::Enter => self.commit_prompt(),
            KeyEvent::Escape|KeyEvent::Ctrl('q') => {self.prompt=None;self.status="Cancelled".into();},
            _=>{}
        }
    }
    fn text(&self)->String { self.lines.join("\n") }
}

fn sanitize_line(s:&str)->String {
    s.chars().take(MAX_LINE).map(|c| if c=='\t' {' '} else if c.is_ascii_graphic()||c==' ' {c}else{'?'}).collect()
}

fn open_or_create(path:&str)->Result<(Arc<dyn VfsNode>,String),VfsError> {
    match crate::vfs::lookup_path(path) {
        Ok(node) if !node.is_directory() => {
            let size=(node.size() as usize).min(MAX_FILE); let mut bytes=alloc::vec![0u8;size]; let n=node.read(0,&mut bytes)?; bytes.truncate(n);
            Ok((node,String::from_utf8_lossy(&bytes).into_owned()))
        }
        Ok(_)=>Err(VfsError::InvalidArgument),
        Err(_) => {
            let trimmed=path.trim_end_matches('/'); let i=trimmed.rfind('/').ok_or(VfsError::InvalidArgument)?; let leaf=&trimmed[i+1..]; if leaf.is_empty(){return Err(VfsError::InvalidArgument)}
            let parent=if i==0{"/"}else{&trimmed[..i]}; let dir=crate::vfs::lookup_path(parent)?; let node=dir.create_file(leaf)?; Ok((node,String::new()))
        }
    }
}

fn write_path(path:&str,data:&[u8])->Result<(),VfsError>{let t=path.trim_end_matches('/');let i=t.rfind('/').ok_or(VfsError::InvalidArgument)?;let parent=if i==0{"/"}else{&t[..i]};let leaf=&t[i+1..];let dir=crate::vfs::lookup_path(parent)?;let file=match dir.lookup(leaf){Ok(n)=>n,Err(VfsError::NotFound)=>dir.create_file(leaf)?,Err(e)=>return Err(e)};file.truncate(0)?;if !data.is_empty()&&file.write(0,data)?!=data.len(){return Err(VfsError::IoError)}file.sync();Ok(())}
fn save(editor:&mut Editor,node:&Arc<dyn VfsNode>)->Result<(),VfsError> {
    if editor.config.backup && node.size()>0 {let size=(node.size() as usize).min(MAX_FILE);let mut old=alloc::vec![0u8;size];let n=node.read(0,&mut old)?;old.truncate(n);write_path(&format!("{}.bak",editor.path),&old)?;}
    let text=if editor.config.trim_trailing {editor.lines.iter().map(|l|l.trim_end()).collect::<Vec<_>>().join("\n")}else{editor.text()}; if text.len()>MAX_FILE{return Err(VfsError::InvalidArgument)}
    node.truncate(0)?; if !text.is_empty(){let n=node.write(0,text.as_bytes())?;if n!=text.len(){return Err(VfsError::IoError)}} node.sync();
    editor.dirty=false;editor.quit_armed=false;editor.status=format!("Saved {} bytes",text.len());Ok(())
}

fn clipped_ascii(s:&str,start:usize,width:usize,spaces:bool)->String { s.bytes().skip(start).take(width).map(|b|if b==b' '&&spaces{'.'}else if (32..=126).contains(&b){b as char}else{'?'}).collect() }

fn render(editor:&Editor) {
    let (w,h)=framebuffer::dimensions(); if w==0||h==0{return}
    let cols=w/CHAR_W; let rows=h.saturating_sub(HEADER_H+FOOTER_H)/CHAR_H; let gutter=if editor.config.line_numbers{7usize}else{0};
    let [bg,surface,text,muted,blue,green,red,select]=editor.config.palette();
    cursor::hide();
    let _=framebuffer::with(|fb| {
        fb.fill_rect(0,0,w,h,bg); fb.fill_rect(0,0,w,HEADER_H,blue);
        let mark=if editor.dirty{" *"}else{""}; let title=format!(" nano+  {}{}",editor.path,mark); fb.draw_text_px(6,3,&clipped_ascii(&title,0,cols.saturating_sub(1),false),0xFFFFFF,blue);
        for vr in 0..rows { let r=editor.top+vr; let y=HEADER_H+vr*CHAR_H; if r>=editor.lines.len(){fb.draw_text_px(8,y,"~",blue,bg);continue}
            if editor.config.line_numbers {let num=format!("{:>5} ",r+1); fb.draw_text_px(0,y,&num,muted,surface);}
            let visible=clipped_ascii(&editor.lines[r],editor.left,cols.saturating_sub(gutter),editor.config.show_whitespace);
            if let Some((hr,hc,hlen))=editor.search_hit { if hr==r && hc>=editor.left && hc<editor.left+visible.len(){ let before=clipped_ascii(&editor.lines[r],editor.left,hc-editor.left,editor.config.show_whitespace); let hit=clipped_ascii(&editor.lines[r],hc,hlen,editor.config.show_whitespace); fb.draw_text_px(gutter*CHAR_W,y,&visible,text,bg); fb.draw_text_px((gutter+before.len())*CHAR_W,y,&hit,0xFFFFFF,select); } else {fb.draw_text_px(gutter*CHAR_W,y,&visible,text,bg);} } else {fb.draw_text_px(gutter*CHAR_W,y,&visible,text,bg);}
        }
        let foot=h-FOOTER_H; fb.fill_rect(0,foot,w,FOOTER_H,surface);
        let status=if let Some((mode,value))=&editor.prompt {format!("{}: {}_",if *mode==PromptMode::Search{"Search"}else{"Line"},value)} else {format!("{}   Ln {}, Col {}",editor.status,editor.row+1,editor.col+1)};
        fb.draw_text_px(6,foot+2,&clipped_ascii(&status,0,cols.saturating_sub(1),false),if editor.status.starts_with("Error"){red}else{green},surface);
        fb.draw_text_px(6,foot+20,"^S Save ^Q Quit ^F Find ^R Replace ^K Cut ^U Paste ^Z Undo",text,surface);
        if editor.prompt.is_none() && editor.row>=editor.top && editor.col>=editor.left { let cx=(gutter+editor.col-editor.left)*CHAR_W; let cy=HEADER_H+(editor.row-editor.top)*CHAR_H; if cx<w {fb.fill_rect(cx,cy+CHAR_H-2,CHAR_W,2,blue);} }
    });
}

pub fn run(path_arg:&str) {
    let path=super::path::resolve(&super::path::cwd(),path_arg);
    let (node,text)=match open_or_create(&path){Ok(v)=>v,Err(e)=>{super::render::error_line(&format!("nano: {}: {:?}",path,e));return}};
    let mut editor=Editor::new(&path,&text,NanoConfig::load()); let mut decoder=Decoder::new();
    crate::kprintln!("nano+: editing {}",path); render(&editor);
    'app: loop {
        crate::arch::cpu::halt();
        while let Some(sc)=super::try_read_scancode(){ let Some(event)=decoder.feed(sc) else{continue};
            if editor.prompt.is_some(){editor.handle_prompt(event);render(&editor);continue}
            match event {
                KeyEvent::Char(c)=>editor.insert(c), KeyEvent::Enter=>editor.newline(), KeyEvent::Backspace=>editor.backspace(), KeyEvent::Delete=>editor.delete(),
                KeyEvent::Left=>editor.left(),KeyEvent::Right=>editor.right(),KeyEvent::Up=>editor.up(),KeyEvent::Down=>editor.down(),KeyEvent::Home=>{editor.col=0;editor.ensure_visible()},KeyEvent::End=>{editor.col=editor.lines[editor.row].len();editor.ensure_visible()},
                KeyEvent::PageUp=>{for _ in 0..12{editor.up()}},KeyEvent::PageDown=>{for _ in 0..12{editor.down()}},
                KeyEvent::Tab=>{for _ in 0..editor.config.tab_size{editor.insert(' ')}}, KeyEvent::Ctrl('s')=>if let Err(e)=save(&mut editor,&node){editor.status=format!("Error saving: {:?}",e)},
                KeyEvent::Ctrl('q')|KeyEvent::Escape=>{if editor.dirty&&!editor.quit_armed{editor.quit_armed=true;editor.status="Unsaved changes — press ^Q again to quit".into()}else{break 'app}},
                KeyEvent::Ctrl('f')=>editor.begin_prompt(PromptMode::Search),KeyEvent::Ctrl('r')=>editor.begin_prompt(PromptMode::ReplaceFind),KeyEvent::Ctrl('g')=>editor.begin_prompt(PromptMode::Goto),KeyEvent::Ctrl('z')=>editor.undo(),KeyEvent::Ctrl('y')=>editor.redo(),KeyEvent::Ctrl('c')=>editor.copy_line(),KeyEvent::Ctrl('k')=>editor.cut_line(),KeyEvent::Ctrl('u')|KeyEvent::Ctrl('v')=>editor.paste_line(),_=>{}
            }
            render(&editor);
        }
    }
    framebuffer::clear_screen(); cursor::hide(); crate::kprintln!("nano+: closed {}",path);
}
