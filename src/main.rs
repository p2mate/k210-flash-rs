extern crate clap;
extern crate elf_rs;
extern crate nix;
extern crate pipefile;
extern crate libc;

mod k210_rom_cmd;
mod slipcodec;

use std::str;
use tokio::sync::mpsc;
use std::io::{Write, Read};
use k210_rom_cmd::K210RomCmd;
use slipcodec::{Error, ErrorKind};
use indicatif::{ProgressBar, ProgressStyle};
use clap::{Arg, App};
use elf_rs::{Elf, ElfMachine, ProgramType};
use tokio_util::codec::Decoder;
use futures::{sink::SinkExt, stream::StreamExt};
use tokio::prelude::*;
use tokio::time;
use bytes::Bytes;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::ffi::OsStr;
use log::{Record, Level, Metadata, info, error, debug};

#[cfg(unix)]
const DEFAULT_TTY: &str = "/dev/ttyUSB0";

struct ColourLogger {
    debug: console::Style,
    info: console::Style,
    warn: console::Style,
    error: console::Style,    
}

impl log::Log for ColourLogger {
    fn enabled(&self, m: &Metadata) -> bool {
        m.level() <= Level::Debug
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            match record.level() {
                log::Level::Error => println!("{}", self.error.apply_to(format!("{}", record.args()))),
                log::Level::Warn => println!("{}", self.warn.apply_to(format!("{}", record.args()))),
                log::Level::Info => println!("{}", self.info.apply_to(format!("{}", record.args()))),
                log::Level::Debug => println!("{}", self.debug.apply_to(format!("{}", record.args()))),
                _ => unimplemented!(),
            }
        }
    }

    fn flush(&self) {}
}

fn debug_print(s: String) {
    if s.len() > 0 {
        debug!("{}", s);
    }
}

async fn load_section(section: &[u8], addr: u32, rom_cmd: &mut K210RomCmd) -> Result<(), Error> {
    let progress_bar = ProgressBar::new(section.len() as u64)
                                .with_style(ProgressStyle::default_bar()
                                .template("{wide_bar} {pos}/{len} {msg}"));
    progress_bar.set_message("bytes loaded");
    let mut address = addr;
    for chunk in section.chunks(1024) {                        
        rom_cmd.memory_write(address, &chunk).await?;
        address += chunk.len() as u32;
        progress_bar.inc(chunk.len() as u64);
    }
    progress_bar.finish();
    Ok(())
}

async fn do_load_image<T: AsRef<str> + ?Sized> (tty_path: &T, file: &T) -> Result<(), Error> {
    let tty_path = tty_path.as_ref();
    let file_path = file.as_ref();
    let mut rom_cmd = K210RomCmd::from_path(tty_path)?;
    let file_name = Path::new(file_path).file_name()
                            .unwrap_or(OsStr::new("Unknown"))
                            .to_string_lossy();
    match rom_cmd.greet().await  {
        Ok(x) => {
                    info!("Handshake with K210 succeeded");
                    debug_print(x);
        },
        Err(x) => {
            error!("Handshake with K210 failed: {}", x);
            return Err(x);
        }
    }

    let mut reader = std::fs::File::open(file_path)?;
    let mut image: Vec<u8> = Vec::new();
    reader.read_to_end(&mut image)?;

    if let Ok(elf) = Elf::from_bytes(&image) {
        if let Elf::Elf64(elf64) = elf {
            if elf64.header().machine() == ElfMachine::RISC_V {
                info!("Loading {}", file_name);
                for p in elf64.program_header_iter() {                   
                    if p.ph.ph_type() == ProgramType::LOAD {
                        let address = p.ph.paddr() as u32;                        
                        let offset = p.ph.offset() as usize;
                        let size = p.ph.filesz() as usize;
                        let section = &image[offset..offset + size];

                        info!("Loading section @ {:x}", address);
                        load_section(section, address, &mut rom_cmd).await?;                    
                    }               
                }
                rom_cmd.memory_boot(elf64.header().entry_point() as u32).await?;
            } else {
                return Err(Error::new(ErrorKind::InvalidInput, "Not a RISCV64 ELF file"));
            }
        }
    } else {
        let address: u32 = 0x80000000;

        info!("Loading image {} @ {:x}", file_name, address);
        load_section(&image, address, &mut rom_cmd).await?;        
        rom_cmd.memory_boot(address).await?;
    }

    Ok(())
}

async fn do_flash_image<T: AsRef<str> + ?Sized> (tty_path: &T, file: &T) -> Result<(), Error> {
    let isp_image = std::include_bytes!("isp-image.bin");
    let mut rom_cmd = K210RomCmd::from_path(&tty_path.as_ref())?;

    match rom_cmd.greet().await  {
        Ok(x) => {
            info!("Handshake with K210 succeeded");
            if x.len() > 0 {
                debug!("{}", x);
            }
        },
        Err(x) => {
            error!("Handshake with K210 failed: {}", x);
            return Err(x);
        }
    }
    
    let address: u32 = 0x80000000;
    info!("Loading ISP loader @ {:x}", address);
    load_section(isp_image, address, &mut rom_cmd).await?;
    rom_cmd.memory_boot(address).await?;

    let mut reader = std::fs::File::open(&file.as_ref())?;
    let mut image: Vec<u8> = Vec::new();
    reader.read_to_end(&mut image)?;
    let image_size = image.len() as u32;

    use sha2::{Sha256, Digest};

    let mut sha256 = Sha256::new();
    sha256.input(&[0 as u8]);
    sha256.input(&image_size.to_le_bytes());    
    sha256.input(&image);
    let sha256_value = sha256.result();
    let final_image = [vec![0], image_size.to_le_bytes().to_vec(), image, sha256_value.to_vec()].concat();

    info!("Waiting for ISP loader to start");
    time::delay_for(time::Duration::from_millis(500)).await;
    let result = rom_cmd.flash_greet().await?;
    debug_print(result);
    
    let result = rom_cmd.select_flash_type(k210_rom_cmd::K210FlashType::External).await?;
    debug_print(result);

    let file_name = Path::new(file.as_ref()).file_name()
                                .unwrap_or(OsStr::new("Unknown"))
                                .to_string_lossy();
    info!("Flashing {}", file_name);
    let progress_bar = ProgressBar::new(image_size as u64)
                                .with_style(ProgressStyle::default_bar()
                                .template("{wide_bar} {pos}/{len} {msg}"));
    progress_bar.set_message("bytes flashed");
    let mut address = 0;
    for chunk in final_image.chunks(4096 * 16) {
        rom_cmd.flash_write(address, &chunk).await?;
        address += chunk.len() as u32;       
        progress_bar.inc(chunk.len() as u64);
    }
    progress_bar.finish();
    rom_cmd.reset_target().await;
    Ok(())        
}

struct RawModeFd {
    attrs: nix::sys::termios::Termios,
    fd: i32,
}

impl RawModeFd {    
    fn new(fd: i32) -> Result<Self, Error> {
        use nix::sys::termios;

        let attrs = termios::tcgetattr(fd).or(Err(Error::new(ErrorKind::Other,"tcgetattr failed")))?;
        let mut raw_attrs = attrs.clone();
        termios::cfmakeraw(&mut raw_attrs);
        termios::tcsetattr(fd, termios::SetArg::TCSANOW, &raw_attrs).or(Err(Error::new(ErrorKind::Other,"tcgetattr failed")))?;
        Ok(RawModeFd { attrs, fd })
    }
}

impl Drop for RawModeFd {
    fn drop(&mut self) {
        use nix::sys::termios;
        dbg!(self.fd);
        let _ = termios::tcsetattr(self.fd, termios::SetArg::TCSANOW, &self.attrs);
    }
}

fn mio_poll_loop(stdin: &mut std::io::StdinLock, pipe_fd: i32, ch_tx: mpsc::UnboundedSender<Vec<u8>>) -> Result<(), Error> {
    use fcntl::{FcntlArg, OFlag};
    use nix::fcntl;

    let stdin_fd = stdin.as_raw_fd();
    fcntl::fcntl(stdin_fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).
            or(Err(Error::new(ErrorKind::Other, "fcntl NONBLOCK failed")))?;
    fcntl::fcntl(pipe_fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).
            or(Err(Error::new(ErrorKind::Other, "fcntl NONBLOCK failed")))?;
    use mio::{Interest, Token};
    use mio::unix::SourceFd;

    let mut events = mio::Events::with_capacity(1024);
    let mut poll = mio::Poll::new()?;
    poll.registry().register(&mut SourceFd(&stdin_fd), Token(0), Interest::READABLE)?;
    poll.registry().register(&mut SourceFd(&pipe_fd), Token(1), Interest::READABLE)?;

    loop {
        poll.poll(&mut events, None)?;

        for event in &events {            
            if event.token() == Token(0) {
                let mut buf: Vec<u8> = vec![0;10];  
                match stdin.read(&mut buf) {
                    Err(_) => { break; },
                    Ok(l) if l > 0 => {
                        if buf.contains(&0x1d) {
                            use nix::sys::signal;
                            let _ = signal::kill(nix::unistd::getpid(), signal::SIGINT);
                            break;
                        }
                        if let Err(_) =  ch_tx.send(buf[0..l].to_owned()) {
                            return Ok(());
                        }
                     },
                    _ => {},
                };
            }
            if event.token() == Token(1) {
                return Ok(());
            }
        }
    }
}

async fn do_terminal<T: AsRef<str> + ?Sized> (tty_path: &T) -> Result<(), Error> {
    let tty_path = tty_path.as_ref();

    let mut settings = tokio_serial::SerialPortSettings::default();
    settings.baud_rate = 115200;
    let port = tokio_serial::Serial::from_path(tty_path, &settings)?;
    let serial_rawfd = port.as_raw_fd();    
    let _serial_raw_mode = RawModeFd::new(serial_rawfd)?;

    let bytes_codec = tokio_util::codec::BytesCodec::new();
    let mut reader = bytes_codec.framed(port);

    info!("Entering terminal mode. Press ctrl-] to quit.");

    let mut stdout = tokio::io::stdout();
    let stdout_rawfd = stdout.as_raw_fd();
    let _term_raw_mode = RawModeFd::new(stdout_rawfd)?;

    let (ch0_tx, mut ch0_rx) = mpsc::unbounded_channel::<Vec<u8>>();      
    let pipe = pipefile::pipe()?;
    let pipe_rx_rawfd = pipe.read_end.as_raw_fd();
    let mut pipe_tx = pipe.write_end;

    let stdin_task = std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut stdin = stdin.lock();
        
        mio_poll_loop(&mut stdin, pipe_rx_rawfd, ch0_tx)     
    });

    loop {
        tokio::select! {
            v = ch0_rx.recv() => {
                match v {
                    Some(c) if c == [0x1d] => break,
                    Some(c) => {
                        if let Err(_) = time::timeout(time::Duration::from_millis(500), reader.send(Bytes::from(c))).await {
                            break;
                        }
                    },
                    _ => break,
                }
            }
            r = reader.next() => {
                match r {
                    Some (b) => {
                        stdout.write_all(&*b?).await?;
                        stdout.flush().await?;
                    },
                    None => { break },
                }
            }
        }
    }
    pipe_tx.write(&[0])?;
    let _ = stdin_task.join().or(Err(Error::new(ErrorKind::Other, "Thread failed")));
    
    Ok(())    
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    
    let logger =  ColourLogger {
        debug: console::Style::new().magenta(),
        info: console::Style::new().green(),
        warn: console::Style::new().yellow(),
        error: console::Style::new().red(),
    };
    
    log::set_boxed_logger(Box::new(logger)).map(|()| log::set_max_level(log::LevelFilter::Debug))
                .or(Err(Error::new(ErrorKind::Other,"set logger failed")))?;

    let cli_args = App::new("k210-flash-rs")
        .version("0.1")
        .author("Peter De Schrijver")
        .about("K210 In System Programmer/Loader")
        .arg(Arg::with_name("PORT")
            .help("Serial port to talk to K210 target")
            .short("s")
            .takes_value(true)
            .long("serial"))
        .arg(Arg::with_name("terminal")
            .help("Start terminal after loading or flashing")
            .short("t")
            .long("terminal")
            .required_unless_one(&["download", "flash"]))
        .arg(Arg::with_name("download")
            .help("Download ELF or binary image")
            .short("d")
            .long("download")
            .takes_value(true)
            .required_unless_one(&["terminal", "flash"]))
        .arg(Arg::with_name("flash")
            .help("Flash binary image")
            .short("f")
            .long("flash")
            .takes_value(true)
            .required_unless_one(&["download", "terminal"])
            .conflicts_with("download"))
        .get_matches();

    let tty_path = if let Some(path) = cli_args.value_of("PORT") {
        path
    } else {
        DEFAULT_TTY.into()
    };
        
    if cli_args.is_present("download") {
        let file_path = cli_args.value_of("download").unwrap();
        do_load_image(&tty_path, &file_path).await?;
    }
    
    if cli_args.is_present("flash") {
        let file_path = cli_args.value_of("flash").unwrap();
        do_flash_image(&tty_path, &file_path).await?;
    }

    if cli_args.is_present("terminal") {
        do_terminal(&tty_path).await?;
    }
    Ok(())
}

#[tokio::test]
async fn test_terminal() {
    do_terminal("/dev/ttyUSB0").await;
}

#[test]
fn test_hash() {
    use sha2::{Sha256, Digest};


    let mut hasher = Sha256::new();

    let mut reader = std::fs::File::open("../k210-sdk-stuff/rust/target/riscv64gc-unknown-none-elf/release/voxel.bin").unwrap();
    let mut image: Vec<u8> = Vec::new();
    reader.read_to_end(&mut image).unwrap();
    let image_size = image.len() as u32;

    hasher.input([0x0]);
    hasher.input(image_size.to_le_bytes());
    hasher.input(image);
    for i in hasher.result() {
        print!("{:x}", i);
    }
    println!("");
}