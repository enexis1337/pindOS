use crate::unit::{ServiceUnit, ServiceState, RestartPolicy, RESTART_LIMIT, RESTART_WINDOW};
use alloc::vec::Vec;

extern crate alloc;

/// -ECHILD от нашего sys_waitpid: процесс уже не отслеживается ядром.
const ECHILD: i64 = -10;

/// Результат неблокирующего sys_waitpid(WNOHANG).
enum WaitResult {
    /// Процесс жив, код не доступен.
    StillRunning,
    /// Процесс завершился, ядро вернуло его код возврата.
    Exited(i32),
    /// Ядро не знает такой pid (-ECHILD).
    NoChild,
}

pub struct ServiceManager {
    units: Vec<ServiceUnit>,
}

impl ServiceManager {
    pub fn new() -> Self {
        Self { units: Vec::new() }
    }

    pub fn register(&mut self, name: &'static str, exec_path: &'static str) {
        self.units.push(ServiceUnit::new(name, exec_path));
    }

    pub fn start_all(&mut self) {
        for i in 0..self.units.len() {
            let exec_path = self.units[i].exec_path;
            match self.spawn(exec_path) {
                Some(pid) => {
                    self.units[i].state = ServiceState::Running;
                    self.units[i].pid   = Some(pid);
                    crate::println!("[dealduck] started {} (pid={})", self.units[i].name, pid);
                }
                None => {
                    self.units[i].state = ServiceState::Failed;
                    crate::println!("[dealduck] FAILED to start {}", self.units[i].name);
                }
            }
        }
    }

    fn spawn(&self, path: &str) -> Option<u32> {
        let pid: i64;
        unsafe {
            core::arch::asm!(
                "syscall",
                in("rax") 2u64,
                in("rdi") path.as_ptr() as u64,
                in("rsi") path.len() as u64,
                lateout("rax") pid,
            );
        }
        if pid < 0 { None } else { Some(pid as u32) }
    }

    pub fn run(&mut self) -> ! {
        loop {
            for i in 0..self.units.len() {
                // Окно перезапусков тикает только пока сервис жив: иначе
                // «упал при старте» и «простоял дольше окна» неразличимы.
                if self.units[i].state == ServiceState::Running {
                    if let Some(pid) = self.units[i].pid {
                        match self.waitpid_nonblock(pid) {
                            WaitResult::Exited(code) => {
                                if code == 0 {
                                    // Штатный выход: OnFailure перезапускать не должен.
                                    self.units[i].state = ServiceState::Stopped;
                                    self.units[i].pid = None;
                                    crate::println!("[dealduck] {} exited cleanly, not restarting", self.units[i].name);
                                } else {
                                    self.units[i].state = ServiceState::Failed;
                                    self.handle_failure(i);
                                }
                            }
                            WaitResult::NoChild => {
                                // -ECHILD: ядро уже не знает про этот pid.
                                self.units[i].state = ServiceState::Failed;
                                self.units[i].pid = None;
                                crate::println!("[dealduck] {}: ECHILD, giving up", self.units[i].name);
                            }
                            WaitResult::StillRunning => {}
                        }
                    }
                } else if self.units[i].window_left > 0 {
                    self.units[i].window_left -= 1;
                    if self.units[i].window_left == 0 {
                        // Окно закрылось без превышения лимита: счётчик сбрасываем.
                        self.units[i].restarts = 0;
                    }
                }
            }

            // Cooperative yield via syscall (syscall 0 = yield)
            unsafe {
                core::arch::asm!(
                    "syscall",
                    in("rax") 0u64,
                );
            }
        }
    }

    /// Решить, перезапускать ли упавший сервис, с учётом RestartPolicy и лимита.
    fn handle_failure(&mut self, i: usize) {
        let name = self.units[i].name;
        crate::println!("[dealduck] {} exited, restarting...", name);

        if self.units[i].restart != RestartPolicy::OnFailure {
            return;
        }

        // Открываем окно, если оно закрыто, и считаем попытку.
        if self.units[i].window_left == 0 {
            self.units[i].restarts = 0;
            self.units[i].window_left = RESTART_WINDOW;
        }

        if self.units[i].restarts >= RESTART_LIMIT {
            self.units[i].state = ServiceState::Failed;
            self.units[i].pid = None;
            crate::println!("[dealduck] {}: start limit reached ({} restarts in {} cycles), not restarting", name, self.units[i].restarts, RESTART_WINDOW);
            return;
        }

        self.units[i].restarts += 1;
        let exec_path = self.units[i].exec_path;
        match self.spawn(exec_path) {
            Some(new_pid) => {
                self.units[i].state = ServiceState::Running;
                self.units[i].pid = Some(new_pid);
            }
            None => {
                self.units[i].state = ServiceState::Failed;
                self.units[i].pid = None;
                crate::println!("[dealduck] FAILED to restart {}", name);
            }
        }
    }

    fn waitpid_nonblock(&self, pid: u32) -> WaitResult {
        let result: i64;
        unsafe {
            core::arch::asm!(
                "syscall",
                in("rax") 3u64,
                in("rdi") pid as u64,
                in("rsi") 1u64,
                lateout("rax") result,
            );
        }
        match result {
            0 => WaitResult::StillRunning,
            r if r == -ECHILD => WaitResult::NoChild,
            r => WaitResult::Exited(r as i32),
        }
    }
}