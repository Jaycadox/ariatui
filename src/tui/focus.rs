#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabKind {
    Current,
    History,
    Scheduler,
    Torrents,
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
            Self::Torrents => "Torrents",
            Self::Routing => "Routing",
            Self::Webhooks => "Webhooks",
            Self::WebUi => "Web UI",
        }
    }

    pub fn all() -> [TabKind; 7] {
        [
            Self::Current,
            Self::History,
            Self::Scheduler,
            Self::Torrents,
            Self::Routing,
            Self::Webhooks,
            Self::WebUi,
        ]
    }

    pub fn next(self) -> Self {
        match self {
            Self::Current => Self::History,
            Self::History => Self::Scheduler,
            Self::Scheduler => Self::Torrents,
            Self::Torrents => Self::Routing,
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
            Self::Torrents => Self::Scheduler,
            Self::Routing => Self::Torrents,
            Self::Webhooks => Self::Routing,
            Self::WebUi => Self::Webhooks,
        }
    }
}
