#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ServiceState {
    Stopped,
    Starting,
    Running,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RestartPolicy {
    No,
    OnFailure,
    Always,
}

/// Максимум перезапусков сервиса в пределах окна `RESTART_WINDOW`.
pub const RESTART_LIMIT: u32 = 5;

/// Сколько тиков цикла мониторинга окно перезапусков остаётся открытым.
pub const RESTART_WINDOW: u32 = 50;

pub struct ServiceUnit {
    pub name:       &'static str,
    pub exec_path:  &'static str,
    pub state:      ServiceState,
    pub restart:    RestartPolicy,
    pub pid:        Option<u32>,
    /// Сколько перезапусков уже сделано в текущем окне.
    pub restarts:   u32,
    /// Осталось циклов, в течение которых окно ещё открыто.
    pub window_left: u32,
}

impl ServiceUnit {
    pub const fn new(name: &'static str, exec_path: &'static str) -> Self {
        Self {
            name,
            exec_path,
            state:   ServiceState::Stopped,
            restart: RestartPolicy::OnFailure,
            pid:     None,
            restarts: 0,
            window_left: 0,
        }
    }
}