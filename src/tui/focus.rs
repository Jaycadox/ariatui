#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabKind {
    Current,
    History,
    Scheduler,
    Routing,
}

impl TabKind {
    pub fn title(self) -> &'static str {
        match self {
            Self::Current => "Current",
            Self::History => "History",
            Self::Scheduler => "Scheduler",
            Self::Routing => "Routing",
        }
    }

    pub fn all() -> [TabKind; 4] {
        [Self::Current, Self::History, Self::Scheduler, Self::Routing]
    }

    pub fn next(self) -> Self {
        match self {
            Self::Current => Self::History,
            Self::History => Self::Scheduler,
            Self::Scheduler => Self::Routing,
            Self::Routing => Self::Current,
        }
    }

    pub fn previous(self) -> Self {
        match self {
            Self::Current => Self::Scheduler,
            Self::History => Self::Current,
            Self::Scheduler => Self::History,
            Self::Routing => Self::Scheduler,
        }
    }
}
