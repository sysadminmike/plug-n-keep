//! The services a bucket can be on, and how each one's own settings (the names its console uses)
//! become the endpoint, region and addressing style the store works with.
#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// Amazon S3: a region, and the endpoint follows from it.
    Aws,
    /// Cloudflare R2: an account ID.
    R2,
    /// Backblaze B2: the endpoint its bucket page shows.
    B2,
    /// Wasabi: a region.
    Wasabi,
    /// A server of your own (RustFS, MinIO, Garage…), or any other S3-compatible one.
    #[default]
    Other,
}

/// What a provider's settings come to, for the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Derived {
    pub endpoint: String,
    pub region: String,
    pub path_style: bool,
}

/// Common AWS regions, for the drop-down (any other can be typed).
pub const AWS_REGIONS: &[&str] = &[
    "us-east-1",
    "us-east-2",
    "us-west-1",
    "us-west-2",
    "ca-central-1",
    "eu-west-1",
    "eu-west-2",
    "eu-west-3",
    "eu-central-1",
    "eu-north-1",
    "eu-south-1",
    "ap-south-1",
    "ap-southeast-1",
    "ap-southeast-2",
    "ap-northeast-1",
    "ap-northeast-2",
    "sa-east-1",
    "af-south-1",
    "me-south-1",
];

/// Wasabi's regions, for the drop-down (any other can be typed).
pub const WASABI_REGIONS: &[&str] = &[
    "us-east-1",
    "us-east-2",
    "us-central-1",
    "us-west-1",
    "ca-central-1",
    "eu-central-1",
    "eu-central-2",
    "eu-west-1",
    "eu-west-2",
    "eu-west-3",
    "ap-northeast-1",
    "ap-northeast-2",
    "ap-southeast-1",
    "ap-southeast-2",
];

impl Provider {
    pub const ALL: [Provider; 5] = [
        Provider::Aws,
        Provider::R2,
        Provider::B2,
        Provider::Wasabi,
        Provider::Other,
    ];

    pub fn is_other(&self) -> bool {
        *self == Provider::Other
    }

    pub fn label(self) -> &'static str {
        match self {
            Provider::Aws => "Amazon S3",
            Provider::R2 => "Cloudflare R2",
            Provider::B2 => "Backblaze B2",
            Provider::Wasabi => "Wasabi",
            Provider::Other => "Other (self-hosted: RustFS, MinIO, Garage…)",
        }
    }

    /// The regions to offer in a drop-down, if this provider asks for a region.
    pub fn regions(self) -> &'static [&'static str] {
        match self {
            Provider::Aws => AWS_REGIONS,
            Provider::Wasabi => WASABI_REGIONS,
            _ => &[],
        }
    }

    /// One line about the keys, in the provider's own words. Never a key itself.
    pub fn keys_help(self) -> &'static str {
        match self {
            Provider::Aws => {
                "In the credentials file: Access key ID and Secret access key (aws_access_key_id, aws_secret_access_key)."
            }
            Provider::R2 => {
                "In the credentials file: the Access Key ID and Secret Access Key of an R2 API token (aws_access_key_id, aws_secret_access_key)."
            }
            Provider::B2 => {
                "In the credentials file: aws_access_key_id = your keyID, aws_secret_access_key = your applicationKey."
            }
            Provider::Wasabi => {
                "In the credentials file: your Access key and Secret key (aws_access_key_id, aws_secret_access_key)."
            }
            Provider::Other => {
                "In the credentials file: aws_access_key_id and aws_secret_access_key for the server."
            }
        }
    }

    /// The endpoint, region and addressing the provider's own settings give. `region` is the
    /// Region field (Amazon S3, Wasabi, Other), `account` the Account ID (R2), `server` the
    /// Endpoint (B2) or Server address (Other).
    pub fn derive(self, region: &str, account: &str, server: &str) -> Result<Derived, String> {
        let region = region.trim();
        let account = account.trim();
        match self {
            Provider::Aws => {
                let region = need_region(region)?;
                Ok(Derived {
                    endpoint: format!("https://s3.{region}.amazonaws.com"),
                    region,
                    path_style: false,
                })
            }
            Provider::Wasabi => {
                let region = need_region(region)?;
                Ok(Derived {
                    endpoint: format!("https://s3.{region}.wasabisys.com"),
                    region,
                    path_style: true,
                })
            }
            Provider::R2 => {
                if account.is_empty() {
                    return Err("Enter the Account ID of your Cloudflare account.".into());
                }
                if !account
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-')
                {
                    return Err("The Account ID can only have letters, digits and dashes.".into());
                }
                Ok(Derived {
                    endpoint: format!("https://{account}.r2.cloudflarestorage.com"),
                    region: "auto".into(),
                    path_style: true,
                })
            }
            Provider::B2 => {
                let endpoint = with_scheme(server)?;
                let host = endpoint
                    .split_once("://")
                    .map_or(endpoint.as_str(), |(_, r)| r)
                    .split('/')
                    .next()
                    .unwrap_or_default();
                let region = host
                    .strip_prefix("s3.")
                    .and_then(|r| r.split('.').next())
                    .filter(|r| !r.is_empty())
                    .ok_or("The endpoint should look like s3.us-west-004.backblazeb2.com.")?
                    .to_string();
                Ok(Derived {
                    endpoint,
                    region,
                    path_style: true,
                })
            }
            Provider::Other => Err("The server's own settings are used as typed.".into()),
        }
    }
}

fn need_region(region: &str) -> Result<String, String> {
    if region.is_empty() {
        return Err("Enter the region, such as us-east-1.".into());
    }
    if !region
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err("The region can only have lower-case letters, digits and dashes.".into());
    }
    Ok(region.to_string())
}

/// `https://` is added when the address has no scheme; a trailing `/` is dropped.
pub fn with_scheme(server: &str) -> Result<String, String> {
    let s = server.trim().trim_end_matches('/');
    if s.is_empty() {
        return Err("Enter the endpoint, as the bucket's page shows it.".into());
    }
    Ok(if s.contains("://") {
        s.to_string()
    } else {
        format!("https://{s}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amazon_s3_gets_its_regional_endpoint_and_virtual_host_style() {
        let d = Provider::Aws.derive(" eu-west-2 ", "", "").unwrap();
        assert_eq!(d.endpoint, "https://s3.eu-west-2.amazonaws.com");
        assert_eq!(d.region, "eu-west-2");
        assert!(!d.path_style);
        assert!(Provider::Aws.derive("", "", "").is_err());
        assert!(Provider::Aws.derive("eu west", "", "").is_err());
    }

    #[test]
    fn r2_uses_the_account_id_and_the_auto_region() {
        let d = Provider::R2.derive("ignored", "abc123", "").unwrap();
        assert_eq!(d.endpoint, "https://abc123.r2.cloudflarestorage.com");
        assert_eq!(d.region, "auto");
        assert!(Provider::R2.derive("", "", "").is_err());
        assert!(Provider::R2.derive("", "a/b", "").is_err());
    }

    #[test]
    fn b2_takes_the_region_from_the_endpoint_and_adds_https() {
        let d = Provider::B2
            .derive("", "", "s3.us-west-004.backblazeb2.com")
            .unwrap();
        assert_eq!(d.endpoint, "https://s3.us-west-004.backblazeb2.com");
        assert_eq!(d.region, "us-west-004");
        let d = Provider::B2
            .derive("", "", "https://s3.eu-central-003.backblazeb2.com/")
            .unwrap();
        assert_eq!(d.endpoint, "https://s3.eu-central-003.backblazeb2.com");
        assert_eq!(d.region, "eu-central-003");
        assert!(Provider::B2.derive("", "", "backblazeb2.com").is_err());
        assert!(Provider::B2.derive("", "", " ").is_err());
    }

    #[test]
    fn wasabi_gets_its_regional_endpoint() {
        let d = Provider::Wasabi.derive("eu-central-1", "", "").unwrap();
        assert_eq!(d.endpoint, "https://s3.eu-central-1.wasabisys.com");
        assert_eq!(d.region, "eu-central-1");
        assert!(Provider::Wasabi.derive("", "", "").is_err());
    }

    #[test]
    fn the_default_provider_is_other_and_is_not_written_out() {
        assert!(Provider::default().is_other());
        assert_eq!(serde_json::to_string(&Provider::R2).unwrap(), "\"r2\"");
        assert_eq!(
            serde_json::from_str::<Provider>("\"wasabi\"").unwrap(),
            Provider::Wasabi
        );
    }
}
