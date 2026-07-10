use serde::Serialize;

use super::Toolchain;
use crate::CoreError;

/// The `compactp` crates we link against.
pub const COMPACTP_VERSION: &str = "0.1.0-beta.1";
/// Lowest Compact language version `compactp` claims to parse.
pub const COMPACTP_MIN_LANGUAGE: &str = "0.23.0";
/// Highest Compact language version `compactp` has actually been validated against.
pub const COMPACTP_TESTED_LANGUAGE: &str = "0.23.0";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skew {
    pub compatible: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Versions {
    /// e.g. `compact 0.5.1` — the CLI, not the compiler.
    pub compact_cli: String,
    /// e.g. `0.31.1`. Can change between invocations; never cache it.
    pub compiler: String,
    pub language: String,
    /// e.g. `ledger-8.0.2` — not bare semver.
    pub ledger: String,
    pub runtime: String,
    pub compactp: String,
    pub skew: Skew,
}

/// Compare the compiler's language version against what `compactp` supports.
/// Pure: no subprocess, no I/O.
pub fn skew_for(language: &str) -> Skew {
    let Ok(lang) = semver::Version::parse(language) else {
        return Skew {
            compatible: false,
            detail: Some(format!(
                "could not parse compiler language version {language:?}"
            )),
        };
    };
    let min = semver::Version::parse(COMPACTP_MIN_LANGUAGE).expect("const is valid semver");
    let tested = semver::Version::parse(COMPACTP_TESTED_LANGUAGE).expect("const is valid semver");

    if lang < min {
        return Skew {
            compatible: false,
            detail: Some(format!(
                "compactp {COMPACTP_VERSION} requires Compact language >= {min}; \
                 compactc reports {lang}. `diagnostics`/`ast`/`symbols` may be wrong."
            )),
        };
    }
    if lang > tested {
        return Skew {
            compatible: true,
            detail: Some(format!(
                "compactp {COMPACTP_VERSION} is validated against language {tested}; \
                 compactc reports {lang}. Parser results may diverge from the compiler."
            )),
        };
    }
    Skew {
        compatible: true,
        detail: None,
    }
}

impl Toolchain {
    pub async fn versions(&self) -> Result<Versions, CoreError> {
        let compact_cli = self.line(&["--version"], false).await?;
        let compiler = self.line(&["--version"], true).await?;
        let language = self.line(&["--language-version"], true).await?;
        let ledger = self.line(&["--ledger-version"], true).await?;
        let runtime = self.line(&["--runtime-version"], true).await?;

        Ok(Versions {
            skew: skew_for(&language),
            compact_cli,
            compiler,
            language,
            ledger,
            runtime,
            compactp: COMPACTP_VERSION.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tested_language_version_is_compatible_and_quiet() {
        let s = skew_for("0.23.0");
        assert!(s.compatible);
        assert_eq!(s.detail, None);
    }

    #[test]
    fn older_language_version_is_incompatible() {
        let s = skew_for("0.22.0");
        assert!(!s.compatible);
        assert!(s.detail.unwrap().contains("0.23.0"));
    }

    #[test]
    fn newer_language_version_is_compatible_but_warns() {
        // compactp declares `>= 0.23` but is only validated against 0.23.0.
        let s = skew_for("0.24.0");
        assert!(s.compatible);
        assert!(s.detail.unwrap().contains("validated against"));
    }

    #[test]
    fn unparseable_language_version_is_reported_not_swallowed() {
        let s = skew_for("banana");
        assert!(!s.compatible);
        assert!(s.detail.unwrap().contains("could not parse"));
    }

    #[tokio::test]
    #[cfg_attr(not(feature = "toolchain-tests"), ignore)]
    async fn versions_reads_the_real_toolchain() {
        let tc = crate::toolchain::Toolchain::new("compact", None);
        let v = tc.versions().await.unwrap();
        assert!(
            v.compact_cli.starts_with("compact "),
            "got {:?}",
            v.compact_cli
        );
        // `--ledger-version` prints e.g. `ledger-8.0.2` — NOT bare semver.
        assert!(v.ledger.starts_with("ledger-"), "got {:?}", v.ledger);
        assert!(semver::Version::parse(&v.language).is_ok());
    }

    #[tokio::test]
    #[cfg_attr(not(feature = "toolchain-tests"), ignore)]
    async fn compiler_version_pin_is_honoured() {
        let tc = crate::toolchain::Toolchain::new("compact", Some("0.31.0".into()));
        let out = tc.run_compile(&["--version"]).await.unwrap();
        assert_eq!(out.stdout.trim(), "0.31.0");
    }
}
