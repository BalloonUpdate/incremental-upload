use std::io::Error;
use std::io::ErrorKind;
use std::process::Command;
use std::io::Result;
use encoding_rs::UTF_8;

pub struct SubprocessTask{
    pub subprocess: Option<Command>,
    pub divided: Vec<String>,
    pub debug: bool,
    pub show_output: bool,
}

impl SubprocessTask {
    pub fn new(subprocess: Option<Command>, divided: Vec<String>, debug: bool, show_output: bool) -> SubprocessTask {
        SubprocessTask { subprocess, divided, debug, show_output }
    }

    pub fn execute(&mut self) -> Result<()> {
        if self.debug {
            println!("> {:?}", self.divided);
        }
    
        if self.subprocess.is_none() {
            return Ok(());
        }

        let result = &mut self.subprocess.take().unwrap()
            .output()
            .expect(&format!("failed to execute command-line: {:?}", self.divided)[..]);
    
        let code = result.status.code();

        match code {
            None => return Err(Error::new(ErrorKind::Interrupted, "process was terminated by a signal.")),
            Some(c) => {
                let stderr = &result.stderr[..];
                let stdout = &result.stdout[..];
                // let stderr = GB18030.decode(stderr).0;
                // let stdout = GB18030.decode(stdout).0;

                let stderr = UTF_8.decode(stderr).0.replace("\r\n", "\n").replace("\r", "\n").trim().replace("\n", "\n|");
                let stdout = UTF_8.decode(stdout).0.replace("\r\n", "\n").replace("\r", "\n").trim().replace("\n", "\n|");

                if c != 0 {
                    println!("\n命令执行失败，返回码({})，以下是详细信息：", c);
                    println!("command-line : {:?}", self.divided);

                    if stdout.trim().len() > 0 {
                        println!("=====stdout=====\n|{}", stdout.trim());
                    }

                    if stderr.trim().len() > 0 {
                        println!("=====stderr=====\n|{}", stderr.trim());
                    }

                    if stdout.trim().len() > 0 || stderr.trim().len() > 0 {
                        println!("================");
                    }

                    return Err(Error::new(ErrorKind::Other, format!("process exited with code: {}.", c)));
                } else if self.show_output {
                    if stdout.trim().len() > 0 {
                        println!("=====stdout=====\n|{}", stdout.trim());
                    }

                    if stderr.trim().len() > 0 {
                        println!("=====stderr=====\n|{}", stderr.trim());
                    }

                    if stdout.trim().len() > 0 || stderr.trim().len() > 0 {
                        println!("================");
                    }
                }
            }
        }
        Ok(())
    }
}
