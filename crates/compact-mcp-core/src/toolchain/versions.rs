use serde::Serialize;
use tokio_util::sync::CancellationToken;

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
    pub async fn versions(&self, ct: &CancellationToken) -> Result<Versions, CoreError> {
        let compact_cli = self.line(&["--version"], false, ct).await?;
        let compiler = self.line(&["--version"], true, ct).await?;
        let language = self.line(&["--language-version"], true, ct).await?;
        let ledger = self.line(&["--ledger-version"], true, ct).await?;
        let runtime = self.line(&["--runtime-version"], true, ct).await?;

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
        let v = tc.versions(&CancellationToken::new()).await.unwrap();
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
        // Derive the expected version from the compiler that is ACTUALLY
        // installed rather than a hard-coded literal, so this can never drift
        // when CI bumps `COMPACT_VERSION` (the mismatch that made issue #15's
        // `toolchain` job red).
        //
        // What this proves: a pin that RESOLVES reports the pinned version
        // back. We pin to the un-pinned default (guaranteed installed) and
        // confirm the pinned `+VERSION` invocation reports that same version.
        // What this does NOT prove on its own: that the `+VERSION` token
        // actually reached `compact compile`. A dropped token would fall back
        // to that same default and still match here. The companion test
        // `a_bogus_compiler_pin_is_rejected_not_silently_defaulted` closes that
        // gap by pinning to a version that is NOT installed.
        let ct = CancellationToken::new();

        // `compact compile --version` with no pin: the default/current
        // compiler, guaranteed to be the one installed by `compact update`.
        let default_version = crate::toolchain::Toolchain::new("compact", None)
            .run_compile(&["--version"], &ct)
            .await
            .unwrap()
            .stdout
            .trim()
            .to_string();
        assert!(
            semver::Version::parse(&default_version).is_ok(),
            "un-pinned `compact compile --version` did not report a semver: {default_version:?}"
        );

        // Pin explicitly to that version and confirm the resolved pin reports
        // it back exactly.
        let pinned = crate::toolchain::Toolchain::new("compact", Some(default_version.clone()));
        let out = pinned.run_compile(&["--version"], &ct).await.unwrap();
        assert_eq!(
            out.stdout.trim(),
            default_version,
            "pinned `compact compile +{default_version} --version` did not honour the pin"
        );
    }

    #[tokio::test]
    #[cfg_attr(not(feature = "toolchain-tests"), ignore)]
    async fn a_bogus_compiler_pin_is_rejected_not_silently_defaulted() {
        // Independently prove the `+VERSION` token actually reaches `compact
        // compile`. Pin to a version that is deliberately NOT installed: the
        // invocation must be REJECTED (non-zero exit, no version on stdout),
        // never silently satisfied by the default compiler.
        //
        // This is the assertion the positive test above cannot make: if
        // `compile_argv` dropped the token, this bogus pin would fall back to
        // the installed default and wrongly succeed with a valid version and a
        // zero exit — which this test would catch. It reintroduces no drift
        // because it asserts a *rejection*, not any specific version. The
        // literal `0.0.0-not-installed` is a valid semver (so it passes the
        // argv builder's shape) that no real release will ever occupy.
        let ct = CancellationToken::new();
        let bogus = crate::toolchain::Toolchain::new("compact", Some("0.0.0-not-installed".into()));
        let out = bogus.run_compile(&["--version"], &ct).await.unwrap();

        assert!(
            out.status != 0,
            "a bogus `+VERSION` pin must exit non-zero, not fall back to the default compiler; \
             got {out:?}"
        );
        assert!(
            semver::Version::parse(out.stdout.trim()).is_err(),
            "a bogus `+VERSION` pin must not report a valid version on stdout; got {:?}",
            out.stdout
        );
    }
}
