use std::{
    collections::VecDeque,
    io,
    pin::Pin,
    process::Stdio,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    sync::{mpsc, oneshot, Mutex as TokioMutex},
};

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
    max_size: usize,
    processes: TokioMutex<VecDeque<Subprocess>>,
    active_count: Arc<AtomicUsize>,
    return_tx: mpsc::Sender<Subprocess>,
    return_rx: TokioMutex<mpsc::Receiver<Subprocess>>,
    builder: Arc<dyn Fn() -> Command + Send + Sync>,
    waiters: (
        mpsc::Sender<oneshot::Sender<()>>,
        TokioMutex<mpsc::Receiver<oneshot::Sender<()>>>,
    ),
}

impl SubprocessPool {
    /// Create a new pool with the given command and arguments.
    /// The pool will start with `max_size` processes.
    pub async fn new(
        builder: impl Fn() -> Command + Send + Sync + 'static,
        max_size: usize,
    ) -> Result<Arc<Self>> {
        let (return_tx, return_rx) = mpsc::channel(max_size);
        let (waiter_tx, waiter_rx) = mpsc::channel(max_size);
        let mut processes = VecDeque::with_capacity(max_size);

        let builder = Arc::new(builder);

        // Create initial processes
        for _ in 0..max_size {
            let process = Subprocess::from_builder(builder.clone())?;
            processes.push_back(process);
        }

        Ok(Arc::new(Self {
            builder,
            max_size,
            processes: TokioMutex::new(processes),
            active_count: Arc::new(AtomicUsize::new(0)),
            return_tx,
            return_rx: TokioMutex::new(return_rx),
            waiters: (waiter_tx, TokioMutex::new(waiter_rx)),
        }))
    }

    /// Get a process from the pool for interactive use.
    /// Waits until a process becomes available if the pool is currently empty.
    pub async fn acquire(self: &Arc<Self>) -> Result<PooledProcess> {
        loop {
            let current = self.active_count.load(Ordering::SeqCst);
            if current >= self.max_size {
                // Create a oneshot channel for this waiter
                let (notify_tx, notify_rx) = oneshot::channel();

                // Queue ourselves
                if self.waiters.0.send(notify_tx).await.is_ok() {
                    // Wait for our turn
                    let _ = notify_rx.await;
                }
                continue;
            }

            if self
                .active_count
                .compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
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
                            pool: self.clone(),
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

    fn spawn_process(&self, processes: &mut VecDeque<Subprocess>) -> Result<()> {
        let process = Subprocess::from_builder(self.builder.clone())?;
        processes.push_back(process);
        Ok(())
    }
}

pub struct PooledProcess {
    process: Option<Subprocess>,
    return_tx: mpsc::Sender<Subprocess>,
    active_count: Arc<AtomicUsize>,
    pool: Arc<SubprocessPool>,
}

impl Drop for PooledProcess {
    fn drop(&mut self) {
        if let Some(process) = self.process.take() {
            if let Ok(()) = self.return_tx.try_send(process) {
                // Try to notify a waiter if there is one
                if let Ok(mut rx) = self.pool.waiters.1.try_lock() {
                    if let Ok(waiter) = rx.try_recv() {
                        let _ = waiter.send(());
                    }
                }
            }
            self.active_count.fetch_sub(1, Ordering::SeqCst);
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

pub struct Subprocess {
    child: Child,
    stdin: Option<TracedStdin>,
    stdout_reader: Option<BufReader<TracedStdout>>,
    stderr_reader: Option<BufReader<TracedStderr>>,
}

impl Subprocess {
    pub fn new(builder: impl Fn() -> Command + 'static) -> Result<Self> {
        Self::from_command(builder())
    }

    pub fn from_builder(builder: Arc<dyn Fn() -> Command>) -> Result<Self> {
        Self::from_command(builder())
    }

    fn from_command(mut command: Command) -> Result<Self> {
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let mut child = command.spawn()?;
        let stdin = child.stdin.take().map(|inner| TracedStdin { inner });
        let stdout_reader = child.stdout.take().map(|inner| BufReader::new(TracedStdout { inner }));
        let stderr_reader = child.stderr.take().map(|inner| BufReader::new(TracedStderr { inner }));

        Ok(Self {
            child,
            stdin,
            stdout_reader,
            stderr_reader,
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
        if !self.is_alive() {
            tracing::debug!("Attempted to write to dead subprocess");
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "Process is no longer alive",
            ));
        }

        if let Some(stdin) = &mut self.stdin {
            tracing::debug!("Writing {} bytes to subprocess stdin", input.len());
            stdin.write_all(input).await?;
            stdin.flush().await?;
            Ok(())
        } else {
            tracing::error!("Subprocess stdin is not available");
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "stdin not available",
            ))
        }
    }

    pub async fn read_bytes(&mut self) -> Result<Vec<u8>> {
        if !self.is_alive() {
            tracing::debug!("Attempted to read from dead subprocess");
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "Process is no longer alive",
            ));
        }

        let mut buf = Vec::new();
        if let Some(stdout) = &mut self.stdout_reader {
            tracing::debug!("Reading from subprocess stdout");
            let bytes_read = stdout.read_to_end(&mut buf).await?;
            tracing::debug!("Read {} bytes from subprocess stdout", bytes_read);
            Ok(buf)
        } else {
            tracing::error!("Subprocess stdout is not available");
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "stdout not available",
            ))
        }
    }

    pub async fn read_bytes_until(&mut self, delimiter: u8) -> Result<Vec<u8>> {
        let mut buf = Vec::new();

        if let Some(mut stdout) = self.stdout_reader.take() {
            tracing::debug!(
                "Reading until delimiter {:?} from subprocess stdout",
                delimiter as char
            );

            loop {
                if !self.is_alive() {
                    if buf.is_empty() {
                        tracing::debug!("Process is dead and no data was read");
                        return Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "Process is no longer alive",
                        ));
                    }
                    // Return what we have if process died but we got some data
                    tracing::debug!("Process died but returning {} bytes of data", buf.len());
                    break;
                }

                let bytes_read = stdout.read_until(delimiter, &mut buf).await?;
                tracing::debug!(
                    "Read {} bytes until delimiter from subprocess stdout",
                    bytes_read
                );

                // If we found the delimiter or got some data, return it
                // if bytes_read > 0 {
                //     break;
                // }
                if buf.last() == Some(&delimiter) {
                    break;
                }

                // No data read but process still alive - wait a bit and try again
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }

            self.stdout_reader = Some(stdout);
            Ok(buf)
        } else {
            tracing::error!("Subprocess stdout is not available");
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "stdout not available",
            ))
        }
    }

    pub fn close_stdin(&mut self) {
        tracing::debug!("Closing subprocess stdin");
        self.stdin.take();
    }
}

impl SubprocessHandler for Subprocess {
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

pub struct TracedStdin {
    inner: ChildStdin,
}

impl tokio::io::AsyncWrite for TracedStdin {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        tracing::debug!("Writing {} bytes to stdin", buf.len());
        let result = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = result {
            tracing::debug!("Wrote {} bytes to stdin", n);
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        tracing::debug!("Flushing stdin");
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        tracing::debug!("Shutting down stdin");
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

pub struct TracedStdout {
    inner: ChildStdout,
}

impl tokio::io::AsyncRead for TracedStdout {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = result {
            let after = buf.filled().len();
            let x = bstr::BStr::new(&buf.filled()[before..after]);
            let bytes_read = after - before;
            tracing::debug!("Read {} bytes from stdout", bytes_read);
            tracing::trace!("Read from stdout: {}", x);
        }
        result
    }
}

pub struct TracedStderr {
    inner: ChildStderr,
}

impl tokio::io::AsyncRead for TracedStderr {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = result {
            let after = buf.filled().len();
            let bytes_read = after - before;
            tracing::debug!("Read {} bytes from stderr", bytes_read);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn get_echo_binary() -> Command {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("target");
        path.push("debug");
        path.push("examples");
        path.push("echo");
        Command::new(path)
    }

    #[tokio::test]
    async fn test_pool_initialization() {
        let pool = SubprocessPool::new(get_echo_binary, 2).await.unwrap();
        assert_eq!(pool.count().await, 2);
    }

    #[tokio::test]
    async fn test_echo_interactive() {
        let pool = SubprocessPool::new(get_echo_binary, 1).await.unwrap();

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
        let pool = SubprocessPool::new(get_echo_binary, 1).await.unwrap();

        let mut process = pool.acquire().await.unwrap();
        process.write_line("test").await.unwrap();
        let response = process.read_line().await.unwrap();
        assert_eq!(response, ">>test\n");
    }

    #[tokio::test]
    async fn test_echo_io_error() {
        let pool = SubprocessPool::new(get_echo_binary, 1).await.unwrap();

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
        let pool = SubprocessPool::new(get_echo_binary, 1).await.unwrap();

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
        let pool = SubprocessPool::new(get_echo_binary, 1).await.unwrap();

        let mut process = pool.acquire().await.unwrap();
        process.write_line("/invalid-utf8").await.unwrap();
        // Give the process time to write the invalid UTF-8 before exiting
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        let result = process.read_line().await;
        assert!(matches!(result, Err(_)));
    }

    #[tokio::test]
    async fn test_pool_exhaustion_and_recreation() {
        let pool = SubprocessPool::new(get_echo_binary, 2).await.unwrap();

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
        process.write_line("final").await.unwrap();
        let response = process.read_line().await.unwrap();
        assert_eq!(response, ">>final\n");
    }

    #[tokio::test]
    async fn test_pool_all_processes_dead() {
        println!("Starting test_pool_all_processes_dead");

        let pool = SubprocessPool::new(get_echo_binary, 2).await.unwrap();
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

        let pool = Arc::new(SubprocessPool::new(get_echo_binary, 5).await.unwrap());
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
        let result = SubprocessPool::new(|| Command::new("\0"), 2).await;
        assert!(matches!(result, Err(_)));
    }

    #[tokio::test]
    async fn test_pool_io_errors() {
        // Create a pool with 'false' command that exits immediately
        let pool = SubprocessPool::new(|| Command::new("false"), 1)
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
    async fn test_pool_high_contention() {
        let pool = SubprocessPool::new(get_echo_binary, 4).await.unwrap();

        // Spawn 100 concurrent tasks all trying to acquire
        let tasks: Vec<_> = (0..100)
            .map(|i| {
                let pool = pool.clone();
                tokio::spawn(async move {
                    println!("Task {} starting", i);

                    // Acquire a process and use it
                    println!("Task {} acquiring process", i);
                    let mut process = pool.acquire().await.unwrap();

                    // Write something unique to verify we got a valid process
                    process.write_line(&format!("task_{}", i)).await.unwrap();
                    let response = process.read_line().await.unwrap();
                    assert_eq!(response.trim(), format!(">>task_{}", i));
                    // Hold the process briefly to increase contention
                    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                })
            })
            .collect();

        // Wait for all tasks to complete
        for task in tasks {
            task.await.unwrap();
        }

        // Verify pool is still in a good state
        let mut process = pool.acquire().await.unwrap();
        process.write_line("final_test").await.unwrap();
        let response = process.read_line().await.unwrap();
        assert_eq!(response.trim(), ">>final_test");
    }

    #[tokio::test]
    async fn test_subprocess_direct() {
        // Create a subprocess directly
        let mut process = Subprocess::new(get_echo_binary).unwrap();

        // Test basic write and read
        process.write_line("hello world").await.unwrap();
        let response = process.read_line().await.unwrap();
        assert_eq!(response.trim(), ">>hello world");

        // Test writing multiple lines
        process.write_line("line 1").await.unwrap();
        process.write_line("line 2").await.unwrap();

        let resp1 = process.read_line().await.unwrap();
        let resp2 = process.read_line().await.unwrap();
        assert_eq!(resp1.trim(), ">>line 1");
        assert_eq!(resp2.trim(), ">>line 2");

        // Test writing raw bytes
        process.write_bytes(b"raw bytes\n").await.unwrap();
        let response = process.read_line().await.unwrap();
        assert_eq!(response.trim(), ">>raw bytes");

        // Test reading until delimiter
        process.write_bytes(b"part1:part2\n").await.unwrap();
        let response = process.read_bytes_until(b':').await.unwrap();
        assert_eq!(&response, b">>part1:");

        // Read the rest
        let response = process.read_line().await.unwrap();
        assert_eq!(response.trim(), "part2");

        // Verify process is still alive
        assert!(process.is_alive());

        // Test closing stdin
        process.close_stdin();
        // Give the process a moment to exit
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        assert!(!process.is_alive());
    }
}
