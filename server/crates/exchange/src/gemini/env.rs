//! Which Gemini this server talks to. Read once at boot from `GEMINI_ENV`.

/// The hosts of one Gemini environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GeminiEnv {
    pub rest_base: &'static str,
    pub ws_url: &'static str,
    pub name: &'static str,
}

pub const PRODUCTION: GeminiEnv = GeminiEnv {
    rest_base: "https://api.gemini.com",
    ws_url: "wss://ws.gemini.com",
    name: "production",
};

pub const SANDBOX: GeminiEnv = GeminiEnv {
    rest_base: "https://api.sandbox.gemini.com",
    ws_url: "wss://ws.sandbox.gemini.com",
    name: "sandbox",
};

impl GeminiEnv {
    /// `production` when `GEMINI_ENV` is unset. Panics at boot on any
    /// value other than `production` or `sandbox`: a typo must not
    /// silently pick an environment.
    pub fn from_env() -> Self {
        let value = std::env::var("GEMINI_ENV").ok();
        match Self::from_name(value.as_deref()) {
            Ok(env) => env,
            Err(e) => panic!("{e}"),
        }
    }

    pub fn from_name(name: Option<&str>) -> Result<Self, String> {
        match name {
            None | Some("") | Some("production") => Ok(PRODUCTION),
            Some("sandbox") => Ok(SANDBOX),
            Some(other) => Err(format!(
                "GEMINI_ENV is {other:?}; it must be \"production\" or \"sandbox\""
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_means_production() {
        assert_eq!(GeminiEnv::from_name(None), Ok(PRODUCTION));
        assert_eq!(GeminiEnv::from_name(Some("")), Ok(PRODUCTION));
    }

    #[test]
    fn the_two_names_pick_their_hosts() {
        assert_eq!(GeminiEnv::from_name(Some("production")), Ok(PRODUCTION));
        let sandbox = GeminiEnv::from_name(Some("sandbox")).unwrap();
        assert_eq!(sandbox.rest_base, "https://api.sandbox.gemini.com");
        assert_eq!(sandbox.ws_url, "wss://ws.sandbox.gemini.com");
    }

    #[test]
    fn anything_else_is_refused_by_name() {
        let err = GeminiEnv::from_name(Some("prod")).unwrap_err();
        assert!(err.contains("prod"), "{err}");
        assert!(GeminiEnv::from_name(Some("Sandbox ")).is_err());
    }
}
