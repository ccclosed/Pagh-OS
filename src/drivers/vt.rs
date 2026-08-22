
//! ANSI/VT-100 terminal emulator rendered on the framebuffer.
#![allow(dead_code)]
use alloc::vec::Vec;
use crate::sync::spinlock::Spinlock;

const DEFAULT_FG: u32 = 0xAAAAAA;
const DEFAULT_BG: u32 = 0x000000;
const GLYPH_W: usize = 8;
const GLYPH_H: usize = 16;
const STATUS_H: usize = 18;

#[derive(Clone, Copy)]
struct Cell { ch: u8, fg: u32, bg: u32 }
impl Cell { const BLANK: Self = Self { ch: b' ', fg: DEFAULT_FG, bg: DEFAULT_BG }; }

#[derive(PartialEq, Clone, Copy)]
enum Es { Normal, Esc, Csi, Osc, OscEsc, Dcs, DcsEsc, Chs }

struct Vt {
    cols: usize, rows: usize, cells: Vec<Cell>,
    cx: usize, cy: usize, saved_cx: usize, saved_cy: usize,
    fg: u32, bg: u32,
    scroll_top: usize, scroll_bot: usize,
    state: Es, params: [u32; 16], np: usize, inter: u8, inter2: u8, osc_buf: Vec<u8>,
}

fn ansi256(n: u32) -> u32 {
    match n {
        0=>0x000000,1=>0xAA0000,2=>0x00AA00,3=>0xAA5500,
        4=>0x0000AA,5=>0xAA00AA,6=>0x00AAAA,7=>0xAAAAAA,
        8=>0x555555,9=>0xFF5555,10=>0x55FF55,11=>0xFFFF55,
        12=>0x5555FF,13=>0xFF55FF,14=>0x55FFFF,15=>0xFFFFFF,
        16..=231 => { let v=n-16; let b=(v%6)*51; let g=((v/6)%6)*51; let r=(v/36)*51; (r<<16)|(g<<8)|b }
        232..=255 => { let v=(n-232)*10+8; (v<<16)|(v<<8)|v }
        _ => DEFAULT_FG,
    }
}

impl Vt {
    fn new(cols: usize, rows: usize) -> Self {
        Self { cols, rows, cells: alloc::vec![Cell::BLANK; cols*rows],
               cx:0,cy:0,saved_cx:0,saved_cy:0,fg:DEFAULT_FG,bg:DEFAULT_BG,
               scroll_top:0,scroll_bot:rows.saturating_sub(1),
               state:Es::Normal,params:[0u32;16],np:0,inter:0,inter2:0,osc_buf:Vec::new() }
    }
    fn idx(&self,x:usize,y:usize)->usize{y*self.cols+x}
    fn get(&self,x:usize,y:usize)->Cell{self.cells[self.idx(x,y)]}
    fn put(&mut self,x:usize,y:usize,c:Cell){
        let i=self.idx(x,y); self.cells[i]=c;
        let px=x*GLYPH_W; let py=STATUS_H+y*GLYPH_H;
        crate::drivers::framebuffer::with(|fb|{fb.draw_glyph_px(c.ch,px,py,c.fg,c.bg);});
    }
    fn paint_all(&self){
        for row in 0..self.rows { for col in 0..self.cols {
            let c=self.get(col,row); let px=col*GLYPH_W; let py=STATUS_H+row*GLYPH_H;
            crate::drivers::framebuffer::with(|fb|{fb.draw_glyph_px(c.ch,px,py,c.fg,c.bg);});
        }}
    }
    fn p(&self,i:usize,def:u32)->u32{if i<self.np&&self.params[i]!=0{self.params[i]}else{def}}
    fn scroll_up(&mut self,n:usize){
        let (top,bot)=(self.scroll_top,self.scroll_bot); if top>=bot{return;}
        let n=n.min(bot-top+1);
        for row in top..=bot.saturating_sub(n){
            for col in 0..self.cols{let c=self.get(col,row+n);let i=self.idx(col,row);self.cells[i]=c;}
        }
        for row in (bot+1).saturating_sub(n)..=bot{
            for col in 0..self.cols{let i=self.idx(col,row);self.cells[i]=Cell::BLANK;}
        }
        self.paint_all();
    }
    fn scroll_dn(&mut self,n:usize){
        let (top,bot)=(self.scroll_top,self.scroll_bot); if top>=bot{return;}
        let n=n.min(bot-top+1);
        for row in (top+n..=bot).rev(){
            for col in 0..self.cols{let c=self.get(col,row-n);let i=self.idx(col,row);self.cells[i]=c;}
        }
        for row in top..top+n{
            for col in 0..self.cols{let i=self.idx(col,row);self.cells[i]=Cell::BLANK;}
        }
        self.paint_all();
    }
    fn erase_disp(&mut self,m:u32){
        let(s,e)=match m{0=>(self.cy*self.cols+self.cx,self.rows*self.cols),1=>(0,self.cy*self.cols+self.cx+1),_=>(0,self.rows*self.cols)};
        for i in s..e.min(self.cells.len()){self.cells[i]=Cell::BLANK;}
        self.paint_all();
    }
    fn erase_line(&mut self,m:u32){
        let(s,e)=match m{0=>(self.cx,self.cols),1=>(0,self.cx+1),_=>(0,self.cols)};
        for col in s..e{self.put(col,self.cy,Cell::BLANK);}
    }
    fn newline(&mut self){
        if self.cy>=self.scroll_bot{self.scroll_up(1);}else{self.cy+=1;}
    }
    fn put_char(&mut self,b:u8){
        if self.cx>=self.cols{self.cx=0;self.newline();}
        let c=Cell{ch:b,fg:self.fg,bg:self.bg};
        self.put(self.cx,self.cy,c); self.cx+=1;
    }
    fn sgr(&mut self){
        let mut i=0;
        while i<self.np.max(1){
            match self.params.get(i).copied().unwrap_or(0){
                0=>{self.fg=DEFAULT_FG;self.bg=DEFAULT_BG;}
                1=>{} // bold - ignored (no bold font)
                30..=37=>{self.fg=ansi256(self.params[i]-30);}
                38=>{
                    match self.params.get(i+1).copied(){
                        Some(5)if i+2<self.np=>{self.fg=ansi256(self.params[i+2]);i+=2;}
                        Some(2)if i+4<self.np=>{let(r,g,b)=(self.params[i+2],self.params[i+3],self.params[i+4]);self.fg=(r<<16)|(g<<8)|b;i+=4;}
                        _=>{}
                    }
                }
                39=>{self.fg=DEFAULT_FG;}
                40..=47=>{self.bg=ansi256(self.params[i]-40);}
                48=>{
                    match self.params.get(i+1).copied(){
                        Some(5)if i+2<self.np=>{self.bg=ansi256(self.params[i+2]);i+=2;}
                        Some(2)if i+4<self.np=>{let(r,g,b)=(self.params[i+2],self.params[i+3],self.params[i+4]);self.bg=(r<<16)|(g<<8)|b;i+=4;}
                        _=>{}
                    }
                }
                49=>{self.bg=DEFAULT_BG;}
                90..=97=>{self.fg=ansi256(self.params[i]-90+8);}
                100..=107=>{self.bg=ansi256(self.params[i]-100+8);}
                _=>{}
            }
            i+=1;
        }
    }
    fn csi(&mut self,f:u8){
        match f{
            b'A'=>{let n=self.p(0,1)as usize;self.cy=self.cy.saturating_sub(n);}
            b'B'=>{let n=self.p(0,1)as usize;self.cy=(self.cy+n).min(self.rows-1);}
            b'C'=>{let n=self.p(0,1)as usize;self.cx=(self.cx+n).min(self.cols-1);}
            b'D'=>{let n=self.p(0,1)as usize;self.cx=self.cx.saturating_sub(n);}
            b'E'=>{let n=self.p(0,1)as usize;self.cy=(self.cy+n).min(self.rows-1);self.cx=0;}
            b'F'=>{let n=self.p(0,1)as usize;self.cy=self.cy.saturating_sub(n);self.cx=0;}
            b'G'=>{self.cx=(self.p(0,1)as usize).saturating_sub(1).min(self.cols-1);}
            b'H'|b'f'=>{
                let r=(self.p(0,1)as usize).saturating_sub(1).min(self.rows-1);
                let c=(self.p(1,1)as usize).saturating_sub(1).min(self.cols-1);
                self.cy=r;self.cx=c;
            }
            b'J'=>{self.erase_disp(self.p(0,0));}
            b'K'=>{self.erase_line(self.p(0,0));}
            b'L'=>{let n=self.p(0,1)as usize;self.scroll_dn(n);}
            b'M'=>{let n=self.p(0,1)as usize;self.scroll_up(n);}
            b'P'=>{
                let n=(self.p(0,1)as usize).min(self.cols-self.cx);
                for col in self.cx..self.cols{
                    let c=if col+n<self.cols{self.get(col+n,self.cy)}else{Cell::BLANK};
                    let i=self.idx(col,self.cy);self.cells[i]=c;
                }
                for col in self.cx..self.cols{self.blit(col,self.cy);}
            }
            b'S'=>{let n=self.p(0,1)as usize;self.scroll_up(n);}
            b'T'=>{let n=self.p(0,1)as usize;self.scroll_dn(n);}
            b'X'=>{let n=(self.p(0,1)as usize).min(self.cols-self.cx);for col in self.cx..self.cx+n{self.put(col,self.cy,Cell::BLANK);}}
            b'd'=>{self.cy=(self.p(0,1)as usize).saturating_sub(1).min(self.rows-1);}
            b'm'=>{self.sgr();}
            b'r'=>{
                let t=(self.p(0,1)as usize).saturating_sub(1);
                let b=(self.p(1,self.rows as u32)as usize).saturating_sub(1).min(self.rows-1);
                if t<b{self.scroll_top=t;self.scroll_bot=b;}
            }
            b's'=>{self.saved_cx=self.cx;self.saved_cy=self.cy;}
            b'u'=>{self.cx=self.saved_cx;self.cy=self.saved_cy;}
            _=>{}
        }
    }
    // Dispatch a completed CSI, answering interrogation
    // sequences. Unknown intermediate/private combos are swallowed silently
    // instead of leaking the final byte as text (the old parser printed
    // "p3m$qm"-style junk out of nvim's DECRQM/DECSCUSR/XTVERSION probes).
    fn csi_final(&mut self,f:u8){
        match(self.inter,self.inter2,f){
            (b'?',b'$',b'p')=>{ // DECRQM (private): "not recognized"
                let s=alloc::format!("\x1b[?{};0$y",self.params[0]);
                respond(s.as_bytes());
            }
            (0,b'$',b'p')=>{ // DECRQM (ANSI)
                let s=alloc::format!("\x1b[{};0$y",self.params[0]);
                respond(s.as_bytes());
            }
            (b'>',0,b'c')=>{respond(b"\x1b[>0;10;1c");} // DA2
            (0,0,b'c')=>{respond(b"\x1b[?6c");}         // DA1: VT102
            (0,0,b'n')=>{match self.params[0]{
                5=>respond(b"\x1b[0n"),                 // DSR: terminal OK
                6=>{ // CPR: cursor position report
                    let s=alloc::format!("\x1b[{};{}R",self.cy+1,self.cx+1);
                    respond(s.as_bytes());
                }
                _=>{}
            }}
            (b'>',0,b'q')=>{respond(b"\x1bP>|PaghVT 16.3\x1b\\");} // XTVERSION
            (0,b' ',b'q')=>{} // DECSCUSR (cursor shape) - ignored
            (b'?',0,b'h')|(b'?',0,b'l')=>{} // DEC private modes - ignored
            (0,0,_)=>{self.csi(f);}
            _=>{} // unknown - swallow, never print the final byte
        }
    }
    fn process_osc(&mut self){
        let buf=core::mem::take(&mut self.osc_buf);
        // OSC 10/11 color queries: nvim probes fg/bg to pick 'background'.
        if buf.starts_with(b"10;?"){respond(b"\x1b]10;rgb:aaaa/aaaa/aaaa\x1b\\");}
        else if buf.starts_with(b"11;?"){respond(b"\x1b]11;rgb:0000/0000/0000\x1b\\");}
    }
    fn blit(&self,x:usize,y:usize){let c=self.get(x,y);let px=x*GLYPH_W;let py=STATUS_H+y*GLYPH_H;crate::drivers::framebuffer::with(|fb|{fb.draw_glyph_px(c.ch,px,py,c.fg,c.bg);});}
    pub fn feed(&mut self,b:u8){
        match self.state{
            Es::Normal=>match b{
                0x07=>{} // BEL
                0x08=>{if self.cx>0{self.cx-=1;}}
                0x09=>{let n=(self.cx+8)&!7;self.cx=n.min(self.cols-1);}
                0x0A|0x0B|0x0C=>{self.cx=0;self.newline();} // STAGE 16.15: LF implies CR (emulated ONLCR)
                0x0D=>{self.cx=0;}
                0x1B=>{self.state=Es::Esc;}
                0x20..=0x7E|0x80..=0xFF=>{self.put_char(b);}
                _=>{}
            },
            Es::Esc=>match b{
                b'['=>{self.state=Es::Csi;self.params=[0u32;16];self.np=0;self.inter=0;self.inter2=0;}
                b']'=>{self.state=Es::Osc;self.osc_buf.clear();}
                // DCS/SOS/PM/APC strings (XTGETTCAP etc.) are
                // consumed up to ST instead of leaking into the grid.
                b'P'|b'X'|b'^'|b'_'=>{self.state=Es::Dcs;}
                b'7'=>{self.saved_cx=self.cx;self.saved_cy=self.cy;self.state=Es::Normal;}
                b'8'=>{self.cx=self.saved_cx;self.cy=self.saved_cy;self.state=Es::Normal;}
                b'M'=>{if self.cy==self.scroll_top{self.scroll_dn(1);}else if self.cy>0{self.cy-=1;}self.state=Es::Normal;}
                // ESC ( X / ESC ) X etc. designate the G0/G1
                // character set. nvim emits ESC(B after every attribute reset;
                // without this state the designator byte leaked onto the screen
                // as a literal 'B' after each highlighted span.
                b'('|b')'|b'*'|b'+'=>{self.state=Es::Chs;}
                _=>{self.state=Es::Normal;}
            },
            Es::Csi=>match b{
                b'0'..=b'9'=>{
                    let i=if self.np==0{self.np=1;0}else{self.np-1};
                    self.params[i.min(15)]=self.params[i.min(15)].saturating_mul(10).saturating_add((b-b'0')as u32);
                }
                b';'|b':'=>{if self.np==0{self.np=1;}if self.np<16{self.np+=1;self.params[self.np-1]=0;}}
                0x3C..=0x3F=>{self.inter=b;}   // private markers: ? > = <
                0x20..=0x2F=>{self.inter2=b;} // Intermediates ($ SP " !)
                0x40..=0x7E=>{
                    self.csi_final(b);
                    self.state=Es::Normal;
                }
                _=>{self.state=Es::Normal;}
            },
            Es::Osc=>{
                if b==0x07{self.process_osc();self.state=Es::Normal;}
                else if b==0x1B{self.state=Es::OscEsc;}
                else if self.osc_buf.len()<64{self.osc_buf.push(b);}
            }
            Es::OscEsc=>{
                if b==b'\\'{self.process_osc();}
                self.state=Es::Normal;
            }
            Es::Dcs=>{
                if b==0x07{self.state=Es::Normal;}
                else if b==0x1B{self.state=Es::DcsEsc;}
            }
            Es::Chs=>{self.state=Es::Normal;} // charset designator byte: consume
            Es::DcsEsc=>{
                self.state=if b==b'\\'{Es::Normal}else{Es::Dcs};
            }
        }
    }
}

// Replies to terminal interrogation sequences (DA1, DSR, DECRQM,
// OSC color queries). nvim sends these at startup and reads the answers from
// stdin; the raw-tty read path drains this queue into STDIN_PENDING.
static RESPONSES: Spinlock<Vec<u8>> = Spinlock::new(Vec::new());

fn respond(bytes:&[u8]){
    RESPONSES.lock().extend_from_slice(bytes);
}

/// True when query replies are queued (drives poll/epoll stdin readability).
pub fn has_input_responses()->bool{ !RESPONSES.lock().is_empty() }

/// Drain queued query replies for injection into the stdin stream.
pub fn take_input_responses()->Vec<u8>{ core::mem::take(&mut *RESPONSES.lock()) }

static VT: Spinlock<Option<Vt>> = Spinlock::new(None);

/// Initialise (or reinitialise) the VT emulator, clearing the screen area.
pub fn init() {
    let (fbw,fbh)=crate::drivers::framebuffer::dimensions();
    if fbw==0||fbh==0{return;}
    let cols=fbw/GLYPH_W; let rows=(fbh.saturating_sub(STATUS_H))/GLYPH_H;
    if cols==0||rows==0{return;}
    crate::drivers::framebuffer::with(|fb|{fb.fill_rect(0,STATUS_H,fbw,fbh-STATUS_H,DEFAULT_BG);});
    let mut vt=Vt::new(cols,rows); vt.paint_all();
    *VT.lock()=Some(vt);
}

/// Returns (cols, rows) of the active VT, or fallback (80,25).
pub fn dimensions()->(u16,u16){
    VT.lock().as_ref().map(|v|(v.cols as u16,v.rows as u16)).unwrap_or((80,25))
}

/// Feed a byte slice from a compat process stdout/stderr into the VT.
pub fn write(bytes:&[u8]){
    let mut g=VT.lock();
    if let Some(vt)=g.as_mut(){for &b in bytes{vt.feed(b);}}
    else{
        drop(g);
        match core::str::from_utf8(bytes){
            Ok(s)=>{crate::fb_print!("{}",s);}
            Err(e)=>{
                // SAFETY: valid_up_to from from_utf8
                let s=unsafe{core::str::from_utf8_unchecked(&bytes[..e.valid_up_to()])};
                crate::fb_print!("{}",s);
            }
        }
    }
}
