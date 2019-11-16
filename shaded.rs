extern crate gpio_cdev;
extern crate bufstream;

use std::io;
use std::io::Write;
use std::io::BufRead;
use bufstream::BufStream;
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use gpio_cdev::{Chip, LineHandle, LineRequestFlags};

const TIME_TO_TOP: Duration = Duration::from_millis(16788);
const TIME_TO_BOTTOM: Duration = Duration::from_millis(16718);
const PRESS_TIME: Duration = Duration::from_millis(100);

enum MovementState {
    MovingUp(Instant),
    MovingDown(Instant),
    Stopped,
}

struct ShadeState {
    movement: MovementState,
    max_pos: u16,
    min_pos: u16,
}

impl Default for ShadeState {
    fn default() -> Self {
        Self {
            movement: MovementState::Stopped,
            max_pos: u16::max_value(),
            min_pos: 0,
        }
    }
}

impl ShadeState {
    fn move_up(&mut self) {
        self.record();
        self.movement = MovementState::MovingUp(Instant::now());
    }
    fn move_down(&mut self) {
        self.record();
        self.movement = MovementState::MovingDown(Instant::now());
    }
    fn stop(&mut self) {
        self.record();
        self.movement = MovementState::Stopped;
    }
    fn record(&mut self) {
        match self.movement {
            MovementState::MovingUp(t) => {
                let dur = t.elapsed();
                // TODO: div_duration_f32?
                let moved = (u16::max_value() as u128 * dur.as_micros()) / TIME_TO_TOP.as_micros();
                let moved = std::cmp::min(moved, u16::max_value() as u128) as u16;
                self.max_pos = self.max_pos.saturating_add(moved);
                self.min_pos = self.min_pos.saturating_add(moved);
                if self.min_pos == 65535 {
                    self.movement = MovementState::Stopped;
                } else {
                    self.movement = MovementState::MovingUp(Instant::now());
                }
            }
            MovementState::MovingDown(t) => {
                let dur = t.elapsed();
                // TODO: div_duration_f32?
                let moved =
                    (u16::max_value() as u128 * dur.as_micros()) / TIME_TO_BOTTOM.as_micros();
                let moved = std::cmp::min(moved, u16::max_value() as u128) as u16;
                self.max_pos = self.max_pos.saturating_sub(moved);
                self.min_pos = self.min_pos.saturating_sub(moved);
                if self.max_pos == 0 {
                    self.movement = MovementState::Stopped;
                } else {
                    self.movement = MovementState::MovingDown(Instant::now());
                }
            }
            MovementState::Stopped => (),
        }
    }
    fn json(&self) -> String {
        let mut result = String::from("{\"state\": \"");
        result.push_str(match self.movement {
            MovementState::MovingUp(_) => "up",
            MovementState::MovingDown(_) => "down",
            MovementState::Stopped => "stopped",
        });
        result.push_str("\",\"max_pos\":");
        result.push_str(&self.max_pos.to_string());
        result.push_str(",\"min_pos\":");
        result.push_str(&self.min_pos.to_string());
        result.push_str(",\"probably\":");
        result.push_str(&((self.max_pos / 2 + self.min_pos / 2).to_string()));
        result.push('}');
        result
    }
}

struct ShadeHandle {
    state: ShadeState,
    handle_up: LineHandle,
    handle_down: LineHandle,
    handle_stop: LineHandle,
}

impl ShadeHandle {
    fn up(&mut self) -> gpio_cdev::errors::Result<()> {
        self.handle_up.set_value(1)?;
        self.state.move_up();
        std::thread::sleep(PRESS_TIME);
        let _ = self.handle_up.set_value(0);
        Ok(())
    }
    fn down(&mut self) -> gpio_cdev::errors::Result<()> {
        self.handle_down.set_value(1)?;
        self.state.move_down();
        std::thread::sleep(PRESS_TIME);
        let _ = self.handle_down.set_value(0);
        Ok(())
    }
    fn stop(&mut self) -> gpio_cdev::errors::Result<()> {
        self.state.record();
        match self.state.movement {
            MovementState::Stopped => { return Ok(()); }
            _ => ()
        };
        self.handle_stop.set_value(1)?;
        self.state.stop();
        std::thread::sleep(PRESS_TIME);
        let _ = self.handle_stop.set_value(0);
        Ok(())
    }
}

fn handle_client(tcp_stream: TcpStream, shade_handle: Arc<Mutex<ShadeHandle>>) {
    if let Ok(peer_addr) = tcp_stream.peer_addr() {
        let mut stream = BufStream::new(tcp_stream);
        println!("Connected: {}", peer_addr);
        let mut line = String::new();
        while let Ok(read_bytes) = stream.read_line(&mut line) {
            if read_bytes == 0 {
                println!("Disconnected: {}", peer_addr);
                break;
            }
            if line == "up\n" {
                shade_handle.lock().unwrap().up().unwrap();
            } else if line == "down\n" {
                shade_handle.lock().unwrap().down().unwrap();
            } else if line == "stop\n" {
                shade_handle.lock().unwrap().stop().unwrap();
            } else {
                shade_handle.lock().unwrap().state.record();
            }
            stream.write(shade_handle.lock().unwrap().state.json().as_bytes()).unwrap();
        }
    }
}

#[derive(Debug)]
enum Error {
    Io(io::Error),
    Gpio(gpio_cdev::errors::Error),
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io(e)
    }
}

impl From<gpio_cdev::errors::Error> for Error {
    fn from(e: gpio_cdev::errors::Error) -> Error {
        Error::Gpio(e)
    }
}

fn main() -> Result<(), Error> {
    let mut chip = Chip::new("/dev/gpiochip0")?;
    println!("Opened GPIO");
    let flags = LineRequestFlags::OUTPUT | LineRequestFlags::ACTIVE_LOW;
    let shade_handle = ShadeHandle {
        state: ShadeState::default(),
        handle_down: chip.get_line(3)?.request(flags, 0, "Shades down")?,
        handle_up: chip.get_line(2)?.request(flags, 0, "Shades up")?,
        handle_stop: chip.get_line(4)?.request(flags, 0, "Shades stop")?,
    };
    println!("Opened lines");
    let shade_handle = Arc::new(Mutex::new(shade_handle));
    let listener = TcpListener::bind("[::]:9911")?;
    println!("Listening on :9911");
    for stream in listener.incoming() {
        match stream {
            Err(e) => println!("accept: {}", e),
            Ok(stream) => {
                let shade_handle = shade_handle.clone();
                thread::spawn(move || {
                    handle_client(stream, shade_handle);
                });
            }
        };
    }
    Ok(())
}
