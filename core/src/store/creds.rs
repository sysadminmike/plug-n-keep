//! S3 credentials: environment variables first, then an AWS-style shared credentials file.
//! Keys are never printed: `Debug` hides them and errors only name the profile and the file.
#![forbid(unsafe_code)]

use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub access_key: String,
    pub secret_key: String,
    pub session_token: Option<String>,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key", &"<hidden>")
            .field("secret_key", &"<hidden>")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<hidden>"),
            )
            .finish()
    }
}

/// The environment the lookup reads, as a value so tests need not touch the real one.
#[derive(Debug, Default, Clone)]
pub struct Env {
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    pub session_token: Option<String>,
    pub shared_file: Option<String>,
    pub profile: Option<String>,
    /// USERPROFILE on Windows, HOME elsewhere.
    pub home: Option<String>,
}

impl Env {
    pub fn from_process() -> Self {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Env {
            access_key_id: var("AWS_ACCESS_KEY_ID"),
            secret_access_key: var("AWS_SECRET_ACCESS_KEY"),
            session_token: var("AWS_SESSION_TOKEN"),
            shared_file: var("AWS_SHARED_CREDENTIALS_FILE"),
            profile: var("AWS_PROFILE"),
            home: var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }),
        }
    }
}

/// Finds the credentials for `profile` (else `AWS_PROFILE`, else `default`). The environment
/// keys win over any file; the file is `file`, else `AWS_SHARED_CREDENTIALS_FILE`, else
/// `~/.aws/credentials`.
pub fn load(profile: Option<&str>, file: Option<&Path>) -> Result<Credentials, String> {
    load_with(&Env::from_process(), profile, file)
}

pub fn load_with(
    env: &Env,
    profile: Option<&str>,
    file: Option<&Path>,
) -> Result<Credentials, String> {
    if let (Some(a), Some(s)) = (&env.access_key_id, &env.secret_access_key) {
        return Ok(Credentials {
            access_key: a.clone(),
            secret_key: s.clone(),
            session_token: env.session_token.clone(),
        });
    }
    let profile = profile
        .filter(|p| !p.is_empty())
        .or(env.profile.as_deref())
        .unwrap_or("default");
    let path: PathBuf = match (file, &env.shared_file, &env.home) {
        (Some(f), _, _) => f.to_path_buf(),
        (None, Some(f), _) => PathBuf::from(f),
        (None, None, Some(h)) => Path::new(h).join(".aws").join("credentials"),
        (None, None, None) => {
            return Err(format!(
                "no S3 credentials for profile \"{profile}\": no credentials file is set and \
                 there is no home folder"
            ));
        }
    };
    let text = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "can't read the credentials file {} for profile \"{profile}\": {}",
            path.display(),
            e.kind()
        )
    })?;
    from_ini(&text, profile)
        .map_err(|why| format!("profile \"{profile}\" in {}: {why}", path.display()))
}

/// Reads `profile` out of the text of a credentials file.
fn from_ini(text: &str, profile: &str) -> Result<Credentials, &'static str> {
    let mut in_profile = false;
    let mut found = false;
    let (mut key, mut secret, mut token) = (None, None, None);
    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let name = name.trim();
            let name = name.strip_prefix("profile ").map_or(name, str::trim);
            in_profile = name == profile;
            if in_profile && found {
                return Err("the profile is listed twice in the file");
            }
            found |= in_profile;
            continue;
        }
        if !in_profile {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim().to_string();
        match k.trim().to_ascii_lowercase().as_str() {
            "aws_access_key_id" => key = Some(v),
            "aws_secret_access_key" => secret = Some(v),
            "aws_session_token" => token = Some(v),
            _ => {}
        }
    }
    if !found {
        return Err("the profile is not in the file");
    }
    match (key, secret) {
        (Some(a), Some(s)) if !a.is_empty() && !s.is_empty() => Ok(Credentials {
            access_key: a,
            secret_key: s,
            session_token: token.filter(|t| !t.is_empty()),
        }),
        _ => Err("it has no access key or no secret key"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "# comment\n; other\n[default]\naws_access_key_id = AKDEF\n\
        aws_secret_access_key=SECDEF\n\n[ other ]\n  aws_access_key_id=AKO  \n\
        aws_secret_access_key = SECO\naws_session_token = TOK\n[profile named]\n\
        aws_access_key_id=AKN\naws_secret_access_key=SECN\n[broken]\naws_access_key_id=X\n";

    #[test]
    fn reads_profiles() {
        let d = from_ini(FILE, "default").unwrap();
        assert_eq!(
            (d.access_key.as_str(), d.secret_key.as_str()),
            ("AKDEF", "SECDEF")
        );
        assert_eq!(d.session_token, None);
        let o = from_ini(FILE, "other").unwrap();
        assert_eq!(o.access_key, "AKO");
        assert_eq!(o.session_token.as_deref(), Some("TOK"));
        assert_eq!(from_ini(FILE, "named").unwrap().access_key, "AKN");
        assert!(from_ini(FILE, "broken").is_err());
        assert!(from_ini(FILE, "missing").is_err());
    }

    #[test]
    fn a_profile_listed_twice_is_refused_not_merged() {
        let twice = "[p]\naws_access_key_id = AK1\n[q]\naws_access_key_id = X\n\
            [p]\naws_secret_access_key = SEC2\n";
        assert!(from_ini(twice, "p").is_err());
        // A profile that is only listed once is fine, whatever else is repeated.
        assert!(from_ini(twice, "q").is_err());
        let once = "[p]\naws_access_key_id = AK1\naws_secret_access_key = S\n[q]\n[q]\n";
        assert!(from_ini(once, "p").is_ok());
    }

    #[test]
    fn debug_hides_keys() {
        let c = from_ini(FILE, "other").unwrap();
        let s = format!("{c:?}");
        for secret in ["AKO", "SECO", "TOK"] {
            assert!(!s.contains(secret), "{s}");
        }
    }

    #[test]
    fn env_keys_win_and_the_file_is_chosen_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("creds");
        std::fs::write(&f, FILE).unwrap();
        let env = Env {
            access_key_id: Some("EK".into()),
            secret_access_key: Some("ES".into()),
            session_token: Some("ET".into()),
            ..Env::default()
        };
        let c = load_with(&env, Some("other"), Some(&f)).unwrap();
        assert_eq!(
            (c.access_key.as_str(), c.session_token.as_deref()),
            ("EK", Some("ET"))
        );
        // Only half the pair set: the file is used.
        let half = Env {
            access_key_id: Some("EK".into()),
            ..Env::default()
        };
        assert_eq!(
            load_with(&half, Some("other"), Some(&f))
                .unwrap()
                .access_key,
            "AKO"
        );
        // Argument, then AWS_SHARED_CREDENTIALS_FILE, then the home folder; profile likewise.
        let env = Env {
            shared_file: Some(f.to_string_lossy().into()),
            profile: Some("named".into()),
            ..Env::default()
        };
        assert_eq!(load_with(&env, None, None).unwrap().access_key, "AKN");
        assert_eq!(
            load_with(&env, Some("other"), None).unwrap().access_key,
            "AKO"
        );
        let aws = dir.path().join(".aws");
        std::fs::create_dir(&aws).unwrap();
        std::fs::write(aws.join("credentials"), FILE).unwrap();
        let env = Env {
            home: Some(dir.path().to_string_lossy().into()),
            ..Env::default()
        };
        assert_eq!(load_with(&env, None, None).unwrap().access_key, "AKDEF");
    }

    #[test]
    fn errors_name_the_profile_and_file_but_no_key() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("creds");
        std::fs::write(&f, FILE).unwrap();
        let e = load_with(&Env::default(), Some("broken"), Some(&f)).unwrap_err();
        assert!(e.contains("broken") && e.contains("creds"), "{e}");
        assert!(!e.contains("AKDEF") && !e.contains("SECDEF"));
        let e = load_with(&Env::default(), Some("p"), Some(&dir.path().join("none"))).unwrap_err();
        assert!(e.contains("\"p\"") && e.contains("none"), "{e}");
        assert!(load_with(&Env::default(), None, None).is_err());
    }
}
