use std::io::{self, BufRead, Write};
use std::process::exit;

fn handle_command(line: &str) -> io::Result<Option<i32>> {
    match line.trim() {
        "/io" => {
            // Simulate IO error by writing error message and returning error
            let stdout = io::stdout();
            let mut stdout = stdout.lock();
            stdout.write_all(b"IO error incoming\n")?;
            stdout.flush()?;
            // Return error
            Err(io::Error::new(io::ErrorKind::Other, "IO error"))
        }
        cmd if cmd.starts_with("/exit ") => {
            if let Ok(code) = cmd[6..].trim().parse() {
                // Write confirmation before exit
                let stdout = io::stdout();
                let mut stdout = stdout.lock();
                writeln!(stdout, "Exiting with code {}", code)?;
                stdout.flush()?;
                exit(code);
            }
            Ok(None)
        }
        "/invalid-utf8" => {
            let stdout = io::stdout();
            let mut stdout = stdout.lock();
            // Write invalid UTF-8 sequence and return error
            stdout.write_all(&[0xFF, 0xFF, 0xFF, 0xFF])?;
            stdout.flush()?;
            Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid UTF-8"))
        }
        _ => Ok(None),
    }
}

fn main() -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdin = stdin.lock();
    let mut stdout = stdout.lock();
    let mut line = String::new();

    loop {
        line.clear();
        if stdin.read_line(&mut line)? == 0 {
            break;
        }

        // Handle special commands
        match handle_command(&line)? {
            Some(code) => exit(code),
            None => {}
        }

        // Echo the line with ">>" prefix
        write!(stdout, ">>{}", line)?;
        stdout.flush()?;
    }

    Ok(())
}
