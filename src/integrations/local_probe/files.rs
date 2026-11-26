use super::{LocalProbeResult, capture, command, memory_file, private_tempdir, read_file};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize, de::IgnoredAny};
use sha2::{Digest, Sha256};
use std::{path::Path, process::Stdio};
use utoipa::ToSchema;

const STRATEGY_MAX_BYTES: usize = 256 * 1024;
const PLAN_MAX_BYTES: usize = 2 * 1024 * 1024;
const MANIFEST_MAX_BYTES: usize = 16 * 1024;
const CSV_MAX_BYTES: usize = 8 * 1024 * 1024;
const CSV_NAME: &str = "bars.csv";

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct StrategyPreview {
    plan_version: PlanVersion,
    execution_model: ExecutionModel,
    #[schema(pattern = "^[0-9a-f]{64}$")]
    source_sha256: String,
    #[schema(pattern = "^[0-9a-f]{64}$")]
    content_sha256: String,
    factors: usize,
    modules: usize,
    sleeves: usize,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, ToSchema)]
enum PlanVersion {
    #[serde(rename = "fertile.finance/strategy-plan/v1")]
    V1,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, ToSchema)]
enum ExecutionModel {
    #[serde(rename = "execution.next_close_cash_equity@1.0.0")]
    NextCloseCashEquity,
}

#[derive(Deserialize)]
struct CompiledPlan {
    plan_version: PlanVersion,
    execution: ExecutionModel,
    source_sha256: String,
    content_sha256: String,
    factors: Vec<IgnoredAny>,
    modules: Vec<IgnoredAny>,
    sleeves: Vec<IgnoredAny>,
}

pub(super) async fn strategy(
    binary: &Path,
    path: &Path,
) -> Result<StrategyPreview, LocalProbeResult> {
    let format = match path.extension().and_then(|ext| ext.to_str()) {
        Some("yaml" | "yml") => "yaml",
        Some("json") => "json",
        _ => return Err(LocalProbeResult::InvalidFile),
    };
    let contents = read_file(path, STRATEGY_MAX_BYTES).await?;
    let input = memory_file(&contents).map_err(|_| LocalProbeResult::ToolUnavailable)?;
    let (status, output) = capture(
        command(binary)
            .args(["compile", "--format", format, "-"])
            .stdin(Stdio::from(input)),
        PLAN_MAX_BYTES,
    )
    .await?;
    if !status.success() {
        return Err(if status.code() == Some(2) {
            LocalProbeResult::ValidationFailed
        } else {
            LocalProbeResult::ToolUnavailable
        });
    }
    let plan: CompiledPlan =
        serde_json::from_slice(&output).map_err(|_| LocalProbeResult::InvalidProducerResponse)?;
    if plan.source_sha256 != sha256(&contents)
        || !is_sha256(&plan.content_sha256)
        || plan.sleeves.is_empty()
    {
        return Err(LocalProbeResult::InvalidProducerResponse);
    }
    Ok(StrategyPreview {
        plan_version: plan.plan_version,
        execution_model: plan.execution,
        source_sha256: plan.source_sha256,
        content_sha256: plan.content_sha256,
        factors: plan.factors.len(),
        modules: plan.modules.len(),
        sleeves: plan.sleeves.len(),
    })
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ResearchPreview {
    instrument: String,
    calendar: Calendar,
    first_session: NaiveDate,
    last_session: NaiveDate,
    observations: u32,
    license: ResearchLicense,
    #[schema(pattern = "^[0-9a-f]{64}$")]
    source_sha256: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum ResearchLicense {
    Synthetic,
    PrivateResearch,
}

impl ResearchLicense {
    fn kind(self) -> DataKind {
        match self {
            Self::Synthetic => DataKind::Fixture,
            Self::PrivateResearch => DataKind::Historical,
        }
    }

    fn probe_source(self) -> &'static str {
        match self {
            Self::Synthetic => "W4 synthetic preflight",
            Self::PrivateResearch => "W4 private preflight",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
enum Calendar {
    #[serde(rename = "XNYS")]
    Xnys,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum AssetType {
    Stock,
    Etf,
}

impl AssetType {
    fn argument(self) -> &'static str {
        match self {
            Self::Stock => "stock",
            Self::Etf => "etf",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum DataKind {
    Fixture,
    Historical,
}

impl DataKind {
    fn argument(self) -> &'static str {
        match self {
            Self::Fixture => "fixture",
            Self::Historical => "historical",
        }
    }
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct Dataset {
    instrument: String,
    asset_type: AssetType,
    source: String,
    source_sha256: String,
    data_kind: DataKind,
    calendar: Calendar,
    first_session: NaiveDate,
    last_session: NaiveDate,
    observations: u32,
    price_basis: PriceBasis,
    corporate_actions: CorporateActions,
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum PriceBasis {
    RawUnadjusted,
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum CorporateActions {
    CashDividendsExcludedSplitFreeReviewed,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResearchManifest {
    license: ResearchLicense,
    dataset: Dataset,
}

impl ResearchManifest {
    fn valid(&self) -> bool {
        let ds = &self.dataset;
        let ticker_is_valid = ds.instrument.strip_suffix(".XNYS").is_some_and(|ticker| {
            (1..=16).contains(&ticker.len())
                && ticker.starts_with(|c: char| c.is_ascii_uppercase())
                && ticker
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-')
        });
        let source_is_valid = (1..=120).contains(&ds.source.len())
            && ds.source.starts_with(|c: char| c.is_ascii_alphanumeric())
            && ds
                .source
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b" ._()/-".contains(&b));
        ticker_is_valid
            && source_is_valid
            && is_sha256(&ds.source_sha256)
            && ds.data_kind == self.license.kind()
            && ds.first_session <= ds.last_session
            && ds.observations != 0
    }
}

pub(super) async fn research(
    python: &Path,
    manifest_path: &Path,
) -> Result<ResearchPreview, LocalProbeResult> {
    let raw = read_file(manifest_path, MANIFEST_MAX_BYTES).await?;
    let mut manifest: ResearchManifest =
        serde_json::from_slice(&raw).map_err(|_| LocalProbeResult::InvalidFile)?;
    if !manifest.valid() {
        return Err(LocalProbeResult::InvalidFile);
    }
    let csv = read_file(&manifest_path.with_file_name(CSV_NAME), CSV_MAX_BYTES).await?;
    if manifest.dataset.source_sha256 != sha256(&csv) {
        return Err(LocalProbeResult::HashMismatch);
    }

    // The finance importer checks actual XNYS sessions and Decimal OHLCV and
    // publishes atomically inside this private disposable root.
    let scratch = private_tempdir(".web-research-probe-", Path::new("/tmp"))
        .map_err(|_| LocalProbeResult::ToolUnavailable)?;
    let input = scratch.path().join(CSV_NAME);
    tokio::fs::write(&input, csv)
        .await
        .map_err(|_| LocalProbeResult::ToolUnavailable)?;
    let probe_source = manifest.license.probe_source();
    let expected = &mut manifest.dataset;
    expected.source = probe_source.into();
    let (status, output) = capture(
        command(python)
            .args(["-I", "-B", "-m", "fertile_finance.research_catalog"])
            .arg(&input)
            .args([
                "--instrument",
                &expected.instrument,
                "--source",
                probe_source,
                "--kind",
                expected.data_kind.argument(),
                "--asset-type",
                expected.asset_type.argument(),
                "--raw-split-free-reviewed",
            ])
            .env("FERTILE_RESEARCH_ROOT", scratch.path().join("catalog"))
            .current_dir(scratch.path()),
        MANIFEST_MAX_BYTES,
    )
    .await?;
    if !status.success() {
        return Err(LocalProbeResult::ValidationFailed);
    }
    let actual: Dataset =
        serde_json::from_slice(&output).map_err(|_| LocalProbeResult::InvalidProducerResponse)?;
    if actual != *expected {
        return Err(LocalProbeResult::InvalidProducerResponse);
    }
    let published = scratch
        .path()
        .join("catalog")
        .join(&actual.instrument)
        .join("dataset.json");
    let staged = read_file(&published, MANIFEST_MAX_BYTES)
        .await
        .map_err(|_| LocalProbeResult::InvalidProducerResponse)?;
    if serde_json::from_slice::<Dataset>(&staged).ok().as_ref() != Some(&actual) {
        return Err(LocalProbeResult::InvalidProducerResponse);
    }
    scratch
        .close()
        .map_err(|_| LocalProbeResult::CleanupFailed)?;
    Ok(ResearchPreview {
        instrument: actual.instrument,
        calendar: actual.calendar,
        first_session: actual.first_session,
        last_session: actual.last_session,
        observations: actual.observations,
        license: manifest.license,
        source_sha256: actual.source_sha256,
    })
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests;
