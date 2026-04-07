#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabKind {
    Current,
    History,
    Scheduler,
    Routing,
    Webhooks,
}

impl TabKind {
    pub fn title(self) -> &'static str {
        match self {
            Self::Current => "Current",
            Self::History => "History",
            Self::Scheduler => "Scheduler",
            Self::Routing => "Routing",
            Self::Webhooks => "Webhooks",
        }
    }

    pub fn all() -> [TabKind; 5] {
        [
            Self::Current,
            Self::History,
            Self::Scheduler,
            Self::Routing,
            Self::Webhooks,
        ]
    }

    pub fn next(self) -> Self {
        match self {
            Self::Current => Self::History,
            Self::History => Self::Scheduler,
            Self::Scheduler => Self::Routing,
            Self::Routing => Self::Webhooks,
            Self::Webhooks => Self::Current,
        }
    }

    pub fn previous(self) -> Self {
        match self {
            Self::Current => Self::Webhooks,
            Self::History => Self::Current,
            Self::Scheduler => Self::History,
            Self::Routing => Self::Scheduler,
            Self::Webhooks => Self::Routing,
        }
    }
}
