#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabKind {
    Current,
    History,
    Scheduler,
    Routing,
    Webhooks,
    WebUi,
}

impl TabKind {
    pub fn title(self) -> &'static str {
        match self {
            Self::Current => "Current",
            Self::History => "History",
            Self::Scheduler => "Scheduler",
            Self::Routing => "Routing",
            Self::Webhooks => "Webhooks",
            Self::WebUi => "Web UI",
        }
    }

    pub fn all() -> [TabKind; 6] {
        [
            Self::Current,
            Self::History,
            Self::Scheduler,
            Self::Routing,
            Self::Webhooks,
            Self::WebUi,
        ]
    }

    pub fn next(self) -> Self {
        match self {
            Self::Current => Self::History,
            Self::History => Self::Scheduler,
            Self::Scheduler => Self::Routing,
            Self::Routing => Self::Webhooks,
            Self::Webhooks => Self::WebUi,
            Self::WebUi => Self::Current,
        }
    }

    pub fn previous(self) -> Self {
        match self {
            Self::Current => Self::WebUi,
            Self::History => Self::Current,
            Self::Scheduler => Self::History,
            Self::Routing => Self::Scheduler,
            Self::Webhooks => Self::Routing,
            Self::WebUi => Self::Webhooks,
        }
    }
}
