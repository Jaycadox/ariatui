use color_eyre::eyre::{Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadUriKind {
    HttpLike,
    Magnet,
}

pub fn classify_download_uri(input: &str) -> Result<DownloadUriKind> {
    let input = input.trim();
    if input.is_empty() {
        bail!("download URI cannot be empty");
    }
    let parsed = reqwest::Url::parse(input)
        .map_err(|_| color_eyre::eyre::eyre!("unsupported or invalid download URI"))?;
    match parsed.scheme() {
        "http" | "https" | "ftp" | "sftp" => Ok(DownloadUriKind::HttpLike),
        "magnet" => Ok(DownloadUriKind::Magnet),
        other => {
            bail!("unsupported URI scheme '{other}' (supported: http, https, ftp, sftp, magnet)")
        }
    }
}

pub fn is_http_like_uri(input: &str) -> bool {
    matches!(classify_download_uri(input), Ok(DownloadUriKind::HttpLike))
}

pub fn magnet_display_name(input: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(input).ok()?;
    if parsed.scheme() != "magnet" {
        return None;
    }
    parsed
        .query_pairs()
        .find(|(key, _)| key == "dn")
        .map(|(_, value)| value.to_string())
        .filter(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_supported_uri_schemes() {
        assert_eq!(
            classify_download_uri("https://example.com/file.iso").unwrap(),
            DownloadUriKind::HttpLike
        );
        assert_eq!(
            classify_download_uri("ftp://example.com/file.iso").unwrap(),
            DownloadUriKind::HttpLike
        );
        assert_eq!(
            classify_download_uri("sftp://example.com/file.iso").unwrap(),
            DownloadUriKind::HttpLike
        );
        assert_eq!(
            classify_download_uri("magnet:?xt=urn:btih:abc&dn=test.torrent").unwrap(),
            DownloadUriKind::Magnet
        );
    }

    #[test]
    fn extracts_magnet_display_name() {
        assert_eq!(
            magnet_display_name("magnet:?xt=urn:btih:abc&dn=ubuntu.iso").as_deref(),
            Some("ubuntu.iso")
        );
    }
}
