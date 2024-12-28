use std::{
    collections::VecDeque,
    io,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{mpsc, Mutex as TokioMutex},
};
use tracing::error;

pub type Result<T> = std::result::Result<T, std::io::Error>;

pub trait SubprocessHandler: Send {
    fn write_bytes(&mut self, input: &[u8])
        -> impl std::future::Future<Output = Result<()>> + Send;

    fn write(&mut self, input: &str) -> impl std::future::Future<Output = Result<()>> + Send {
        async { self.write_bytes(input.as_bytes()).await }
    }

    fn write_line(&mut self, input: &str) -> impl std::future::Future<Output = Result<()>> + Send {
        async move {
            self.write_bytes(input.as_bytes()).await?;
            self.write_bytes(b"\n").await?;
            Ok(())
        }
    }

    fn read_bytes(&mut self) -> impl std::future::Future<Output = Result<Vec<u8>>> + Send;

    fn read_bytes_until(
        &mut self,
        delimiter: u8,
    ) -> impl std::future::Future<Output = Result<Vec<u8>>> + Send;

    fn read(&mut self) -> impl std::future::Future<Output = Result<String>> + Send {
        async {
            let bytes = self.read_bytes().await?;
            String::from_utf8(bytes)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid UTF-8 sequence"))
        }
    }

    fn read_until(
        &mut self,
        delimiter: u8,
    ) -> impl std::future::Future<Output = Result<String>> + Send {
        async move {
            let bytes = self.read_bytes_until(delimiter).await?;
            String::from_utf8(bytes)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid UTF-8 sequence"))
        }
    }

    fn read_line(&mut self) -> impl std::future::Future<Output = Result<String>> + Send {
        async {
            let bytes = self.read_bytes_until(b'\n').await?;
            String::from_utf8(bytes)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid UTF-8 sequence"))
        }
    }

    fn is_alive(&mut self) -> bool;

    fn close_stdin(&mut self);
}

pub struct SubprocessPool {
    cmd: String,
    args: Vec<String>,
    max_size: usize,
    processes: TokioMutex<VecDeque<BasicSubprocessHandler>>,
    active_count: Arc<AtomicUsize>,
    return_tx: mpsc::Sender<BasicSubprocessHandler>,
    return_rx: TokioMutex<mpsc::Receiver<BasicSubprocessHandler>>,
}

impl SubprocessPool {
    /// Create a new pool with the given command and arguments.
    /// The pool will start with `max_size` processes.
    pub async fn new(cmd: String, args: Vec<String>, max_size: usize) -> Result<Arc<Self>> {
        let (return_tx, return_rx) = mpsc::channel(max_size);
        let mut processes = VecDeque::with_capacity(max_size);

        // Create initial processes
        for _ in 0..max_size {
            let process = BasicSubprocessHandler::new(&cmd, &args)?;
            processes.push_back(process);
        }

        Ok(Arc::new(Self {
            cmd,
            args,
            max_size,
            processes: TokioMutex::new(processes),
            active_count: Arc::new(AtomicUsize::new(0)),
            return_tx,
            return_rx: TokioMutex::new(return_rx),
        }))
    }

    /// Write a line to any available process in the pool.
    /// If no process is available, this will block until one becomes available.
    pub async fn write_line(&self, input: &str) -> Result<()> {
        let mut process = self.acquire().await?;
        process.write_line(input).await?;
        Ok(())
    }

    /// Get a process from the pool for interactive use.
    /// Waits until a process becomes available if the pool is currently empty.
    pub async fn acquire(&self) -> Result<PooledProcess> {
        loop {
            // First check the active count
            let current = self.active_count.load(Ordering::SeqCst);
            if current >= self.max_size {
                // Pool is full, wait and try again
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                continue;
            }

            // Try to increment the counter
            if self
                .active_count
                .compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                // Someone else changed it, try again
                continue;
            }

            // Check for any returned processes first
            {
                let mut rx = self.return_rx.lock().await;
                let mut processes = self.processes.lock().await;

                // Get any returned processes
                while let Ok(process) = rx.try_recv() {
                    processes.push_back(process);
                }

                // Remove dead processes first
                processes.retain_mut(|p| p.is_alive());

                // Spawn new processes if needed
                while processes.len() < self.max_size {
                    match self.spawn_process(&mut processes) {
                        Ok(_) => {}
                        Err(e) => {
                            // If we failed to spawn a process, decrement the active count
                            self.active_count.fetch_sub(1, Ordering::SeqCst);
                            return Err(e);
                        }
                    }
                }

                // Take a process if available
                if let Some(mut process) = processes.pop_front() {
                    // Release locks before checking if alive
                    drop(processes);
                    drop(rx);

                    // Verify the process is still alive
                    if process.is_alive() {
                        return Ok(PooledProcess {
                            process: Some(process),
                            return_tx: self.return_tx.clone(),
                            active_count: self.active_count.clone(),
                        });
                    }

                    // Process died, try again (counter already decremented)
                    continue;
                }

                // No processes available, release locks and try again
                drop(processes);
                drop(rx);
            }

            // No processes available, decrement counter and try again
            self.active_count.fetch_sub(1, Ordering::SeqCst);
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }
    }

    /// Get the current number of processes in the pool
    pub async fn count(&self) -> usize {
        // First check for any returned processes
        let mut rx = self.return_rx.lock().await;
        let mut processes = self.processes.lock().await;

        // Get any returned processes
        while let Ok(process) = rx.try_recv() {
            processes.push_back(process);
        }

        // Remove dead processes and spawn new ones if needed
        processes.retain_mut(|p| p.is_alive());
        while processes.len() < self.max_size {
            match self.spawn_process(&mut processes) {
                Ok(_) => {}
                Err(_) => break,
            }
        }

        processes.len()
    }

    fn spawn_process(&self, processes: &mut VecDeque<BasicSubprocessHandler>) -> Result<()> {
        let process = BasicSubprocessHandler::new(&self.cmd, &self.args)?;
        processes.push_back(process);
        Ok(())
    }
}

pub struct PooledProcess {
    process: Option<BasicSubprocessHandler>,
    return_tx: mpsc::Sender<BasicSubprocessHandler>,
    active_count: Arc<AtomicUsize>,
}

impl Drop for PooledProcess {
    fn drop(&mut self) {
        // Take ownership of the process
        let mut process = self.process.take().unwrap();

        // Always decrement active count first
        self.active_count.fetch_sub(1, Ordering::SeqCst);

        // Only return process if it's still alive
        if process.is_alive() {
            // Try to return the process to the pool
            if self.return_tx.try_send(process).is_err() {
                // If we can't return it, just let it drop
                error!("Failed to return process to pool");
            }
        }
    }
}

impl SubprocessHandler for PooledProcess {
    async fn write_bytes(&mut self, input: &[u8]) -> Result<()> {
        let Some(process) = self.process.as_mut() else {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "No process available",
            ));
        };
        process.write_bytes(input).await
    }

    async fn read_bytes(&mut self) -> Result<Vec<u8>> {
        let Some(process) = self.process.as_mut() else {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "No process available",
            ));
        };
        process.read_bytes().await
    }

    async fn read_bytes_until(&mut self, delimiter: u8) -> Result<Vec<u8>> {
        let Some(process) = self.process.as_mut() else {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "No process available",
            ));
        };
        process.read_bytes_until(delimiter).await
    }

    fn is_alive(&mut self) -> bool {
        self.process.as_mut().map(|x| x.is_alive()).unwrap_or(false)
    }

    fn close_stdin(&mut self) {
        if let Some(process) = self.process.as_mut() {
            process.close_stdin()
        }
    }
}

pub struct BasicSubprocessHandler {
    child: Child,
    stdout_reader: Option<BufReader<tokio::process::ChildStdout>>,
}

impl BasicSubprocessHandler {
    pub fn new(cmd: &str, args: &[String]) -> Result<Self> {
        let mut command = Command::new(cmd);
        command.args(args);
        command.stdin(std::process::Stdio::piped());
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        let mut child = command.spawn()?;

        let stdout_reader = child.stdout.take().map(BufReader::new);
        Ok(Self {
            child,
            stdout_reader,
        })
    }

    pub fn is_alive(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(None) => true,
            Ok(Some(_)) => false,
            Err(_) => false,
        }
    }

    pub async fn write_bytes(&mut self, input: &[u8]) -> Result<()> {
        if let Some(stdin) = self.child.stdin.as_mut() {
            stdin.write_all(input).await?;
            stdin.flush().await?;
            if !self.is_alive() {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "Process exited during write",
                ));
            }
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "stdin is closed"))
        }
    }

    pub async fn read_bytes(&mut self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        if let Some(stdout) = self.stdout_reader.as_mut() {
            let bytes_read = stdout.read_to_end(&mut buf).await?;
            if bytes_read == 0 && !self.is_alive() {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "Process exited during read",
                ));
            }
            Ok(buf)
        } else {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "stdout is closed",
            ))
        }
    }

    pub async fn read_bytes_until(&mut self, delimiter: u8) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        if let Some(stdout) = self.stdout_reader.as_mut() {
            let bytes_read = stdout.read_until(delimiter, &mut buf).await?;
            if bytes_read == 0 && !self.is_alive() {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "Process exited during read",
                ));
            }
            Ok(buf)
        } else {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "stdout is closed",
            ))
        }
    }

    pub fn close_stdin(&mut self) {
        self.child.stdin.take();
    }
}

impl SubprocessHandler for BasicSubprocessHandler {
    async fn write(&mut self, input: &str) -> Result<()> {
        self.write_bytes(input.as_bytes()).await
    }

    async fn write_line(&mut self, input: &str) -> Result<()> {
        self.write_bytes(input.as_bytes()).await?;
        self.write_bytes(b"\n").await?;
        Ok(())
    }

    async fn read(&mut self) -> Result<String> {
        let bytes = self.read_bytes().await?;
        String::from_utf8(bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid UTF-8 sequence"))
    }

    async fn read_line(&mut self) -> Result<String> {
        let bytes = self.read_bytes_until(b'\n').await?;
        String::from_utf8(bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid UTF-8 sequence"))
    }

    async fn write_bytes(&mut self, input: &[u8]) -> Result<()> {
        self.write_bytes(input).await
    }

    async fn read_bytes(&mut self) -> Result<Vec<u8>> {
        self.read_bytes().await
    }

    async fn read_bytes_until(&mut self, delimiter: u8) -> Result<Vec<u8>> {
        self.read_bytes_until(delimiter).await
    }

    fn is_alive(&mut self) -> bool {
        self.is_alive()
    }

    fn close_stdin(&mut self) {
        self.close_stdin()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn get_echo_binary() -> (String, Vec<String>) {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("target");
        path.push("debug");
        path.push("examples");
        path.push("echo");
        (path.to_string_lossy().to_string(), vec![])
    }

    #[tokio::test]
    async fn test_pool_initialization() {
        let (cmd, args) = get_echo_binary();
        let pool = SubprocessPool::new(cmd, args, 2).await.unwrap();
        assert_eq!(pool.count().await, 2);
    }

    #[tokio::test]
    async fn test_pool_write() {
        let (cmd, args) = get_echo_binary();
        let pool = SubprocessPool::new(cmd, args, 2).await.unwrap();
        assert!(pool.write_line("test").await.is_ok());
    }

    #[tokio::test]
    async fn test_echo_interactive() {
        let (cmd, args) = get_echo_binary();
        let pool = SubprocessPool::new(cmd, args, 1).await.unwrap();

        let mut process = pool.acquire().await.unwrap();

        // First interaction
        process.write_line("hello").await.unwrap();
        let output = process.read_line().await.unwrap();
        assert_eq!(output, ">>hello\n");

        // Second interaction
        process.write_line("world").await.unwrap();
        let output = process.read_line().await.unwrap();
        assert_eq!(output, ">>world\n");
    }

    #[tokio::test]
    async fn test_write_line() {
        let (cmd, args) = get_echo_binary();
        let pool = SubprocessPool::new(cmd, args, 1).await.unwrap();

        let mut process = pool.acquire().await.unwrap();
        process.write_line("test").await.unwrap();
        let response = process.read_line().await.unwrap();
        assert_eq!(response, ">>test\n");
    }

    #[tokio::test]
    async fn test_echo_io_error() {
        let (cmd, args) = get_echo_binary();
        let pool = SubprocessPool::new(cmd, args, 1).await.unwrap();

        let mut process = pool.acquire().await.unwrap();
        process.write_line("/io").await.unwrap();
        let response = process.read_line().await.unwrap();
        assert_eq!(response, "IO error incoming\n");

        // Give the process a moment to handle the error
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Next operation should fail
        let result = process.write_line("test").await;
        assert!(matches!(result, Err(_)));
    }

    #[tokio::test]
    async fn test_echo_exit_code() {
        let (cmd, args) = get_echo_binary();
        let pool = SubprocessPool::new(cmd, args, 1).await.unwrap();

        let mut process = pool.acquire().await.unwrap();
        process.write_line("/exit 42").await.unwrap();
        let response = process.read_line().await.unwrap();
        assert_eq!(response, "Exiting with code 42\n");

        // Give the process a moment to exit
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Next operation should fail since process exited
        let result = process.write_line("test").await;
        assert!(matches!(result, Err(_)));
    }

    #[tokio::test]
    async fn test_echo_invalid_utf8() {
        let (cmd, args) = get_echo_binary();
        let pool = SubprocessPool::new(cmd, args, 1).await.unwrap();

        let mut process = pool.acquire().await.unwrap();
        process.write_line("/invalid-utf8").await.unwrap();
        // Give the process time to write the invalid UTF-8 before exiting
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        let result = process.read_line().await;
        assert!(matches!(result, Err(_)));
    }

    #[tokio::test]
    async fn test_pool_exhaustion_and_recreation() {
        let (cmd, args) = get_echo_binary();
        let pool = SubprocessPool::new(cmd, args, 2).await.unwrap();
        let initial_count = pool.count().await;
        assert_eq!(initial_count, 2, "Pool should start with max size");

        // Kill all processes by making them exit
        let mut process1 = pool.acquire().await.unwrap();
        let mut process2 = pool.acquire().await.unwrap();

        process1.write_line("/exit 1").await.unwrap();
        process2.write_line("/exit 1").await.unwrap();

        // Drop the processes to return them to the pool
        drop(process1);
        drop(process2);

        // Give processes time to exit and pool time to recreate them
        for i in 1..=20 {
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            let count = pool.count().await;
            println!("After {}ms: pool has {} processes", i * 500, count);
            if count == 2 {
                break;
            }
        }

        // Pool should be back at max size
        let final_count = pool.count().await;
        assert_eq!(final_count, 2, "Pool should be back at max size");

        // Verify we can still acquire and use a process
        let mut process = pool.acquire().await.unwrap();
        process.write_line("test").await.unwrap();
        let response = process.read_line().await.unwrap();
        assert_eq!(response, ">>test\n");
    }

    #[tokio::test]
    async fn test_pool_all_processes_dead() {
        println!("Starting test_pool_all_processes_dead");
        let (cmd, args) = get_echo_binary();
        let pool = SubprocessPool::new(cmd, args, 2).await.unwrap();
        println!("Pool created with 2 processes");

        // Kill all processes by making them exit
        let mut process1 = pool.acquire().await.unwrap();
        let mut process2 = pool.acquire().await.unwrap();
        println!("Acquired both processes");

        process1.write_line("/exit 1").await.unwrap();
        process2.write_line("/exit 1").await.unwrap();
        println!("Sent exit commands to both processes");

        // Drop the processes to return them to the pool
        drop(process1);
        drop(process2);
        println!("Dropped processes");

        // Give processes time to exit and pool time to recreate them
        for i in 1..=20 {
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            let count = pool.count().await;
            println!("After {}ms: pool has {} processes", i * 500, count);
            if count == 2 {
                break;
            }
        }

        println!("Checking pool size");
        // Pool should recreate processes
        let final_count = pool.count().await;
        assert_eq!(final_count, 2, "Pool should recreate all processes");

        println!("Testing final process");
        // Verify we can still acquire and use a process
        let mut process = pool.acquire().await.unwrap();
        println!("Got final process");
        process.write_line("final test").await.unwrap();
        println!("Wrote to final process");
        let response = process.read_line().await.unwrap();
        println!("Got final response: {}", response);
        assert_eq!(response, ">>final test\n");
        println!("Test complete");
    }

    #[tokio::test]
    async fn test_pool_multiple_acquires() {
        println!("Starting test");
        let (cmd, args) = get_echo_binary();
        let pool = Arc::new(SubprocessPool::new(cmd, args, 5).await.unwrap());
        let concurrent_tasks = Arc::new(AtomicUsize::new(0));
        println!("Pool created");

        // Create 10 tasks that will each acquire a process and write to it
        let mut handles = Vec::new();
        for i in 0..10 {
            let pool = pool.clone();
            let concurrent_tasks = concurrent_tasks.clone();
            handles.push(tokio::spawn(async move {
                println!("Task {} starting", i);

                // Acquire a process and use it
                println!("Task {} acquiring process", i);
                let mut process = pool.acquire().await.unwrap();

                // Track number of concurrent tasks AFTER acquiring the process
                let prev_count = concurrent_tasks.fetch_add(1, Ordering::SeqCst);
                println!("Task {} got count {}", i, prev_count);
                assert!(
                    prev_count < 5,
                    "Should never have more than 5 concurrent tasks"
                );
                println!("Task {} got process", i);

                // Write to the process and verify response
                println!("Task {} writing", i);
                process.write_line(&format!("Task {}", i)).await.unwrap();
                println!("Task {} reading", i);
                let response = process.read_line().await.unwrap();
                println!("Task {} got response: {}", i, response);
                assert_eq!(response, format!(">>Task {}\n", i));

                // Release the process and decrement task count
                println!("Task {} releasing process", i);
                concurrent_tasks.fetch_sub(1, Ordering::SeqCst);
                drop(process);
                println!("Task {} done", i);
            }));
        }

        println!("Waiting for tasks to complete");
        for handle in handles {
            handle.await.unwrap();
        }

        println!("All tasks complete");
        // All tasks should be done
        assert_eq!(
            concurrent_tasks.load(Ordering::SeqCst),
            0,
            "All tasks should be complete"
        );
    }

    #[tokio::test]
    async fn test_pool_spawn_failure() {
        // Try to create a pool with a non-existent binary
        let result = SubprocessPool::new("nonexistent_binary".to_string(), vec![], 2).await;
        assert!(matches!(result, Err(_)));
    }

    #[tokio::test]
    async fn test_pool_io_errors() {
        // Create a pool with 'false' command that exits immediately
        let pool = SubprocessPool::new("false".to_string(), vec![], 1)
            .await
            .unwrap();
        let mut process = pool.acquire().await.unwrap();

        // Give process time to exit
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Try to write - should fail since process exited
        let write_result = process.write_line("test").await;
        assert!(matches!(write_result, Err(_)));

        // Try to read - should fail since process exited
        let read_result = process.read_line().await;
        assert!(matches!(read_result, Err(_)));
    }

    #[tokio::test]
    async fn test_pool_write_line() {
        let (cmd, args) = get_echo_binary();
        let pool = SubprocessPool::new(cmd, args, 2).await.unwrap();
        assert!(pool.write_line("test").await.is_ok());
    }
}
