use anyhow::{bail, Context, Result};
use quick_xml::{
    events::{BytesStart, Event},
    Reader,
};
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::BufReader,
    path::Path,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GtestCase {
    pub name: String,
    pub skipped: bool,
    pub failed: bool,
    pub error: bool,
    pub disabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GtestReport {
    pub total: usize,
    pub failures: usize,
    pub errors: usize,
    pub disabled: usize,
    pub skipped: usize,
    pub cases: Vec<GtestCase>,
}

#[derive(Debug)]
struct PendingCase {
    name: String,
    status: String,
    result: String,
    has_skipped: bool,
    has_failure: bool,
    has_error: bool,
}

pub fn parse_gtest_xml(path: &Path) -> Result<GtestReport> {
    let file =
        File::open(path).with_context(|| format!("无法打开 gtest XML: {}", path.display()))?;
    let mut reader = Reader::from_reader(BufReader::new(file));
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut aggregate: Option<(usize, usize, usize, usize, Option<usize>)> = None;
    let mut pending: Option<PendingCase> = None;
    let mut cases = Vec::new();
    let mut names = HashSet::new();
    let mut depth = 0usize;
    let mut root_open = false;
    let mut root_closed = false;

    loop {
        let event = reader
            .read_event_into(&mut buf)
            .context("解析 gtest XML 失败")?;

        match &event {
            Event::Start(element) => {
                let name = element.name();
                if name.as_ref() == b"testsuites" {
                    if depth != 0 || root_open || root_closed {
                        bail!("gtest XML testsuites 根节点位置非法");
                    }
                    root_open = true;
                } else if depth == 0 {
                    bail!("gtest XML 根节点必须是 testsuites");
                }
                if name.as_ref() == b"testcase" && (!root_open || root_closed || depth != 2) {
                    bail!("gtest XML testcase 不在 testsuite 内");
                }
                depth += 1;
            }
            Event::Empty(element) => {
                if element.name().as_ref() == b"testsuites" || depth == 0 {
                    bail!("gtest XML 根节点结构非法");
                }
                if element.name().as_ref() == b"testcase"
                    && (!root_open || root_closed || depth != 2)
                {
                    bail!("gtest XML testcase 不在 testsuite 内");
                }
            }
            Event::End(element) => {
                if depth == 0 {
                    bail!("gtest XML 出现多余结束标签");
                }
                if element.name().as_ref() == b"testcase" && depth != 3 {
                    bail!("gtest XML testcase 结束标签层级非法");
                }
                depth -= 1;
                if element.name().as_ref() == b"testsuites" {
                    if depth != 0 || !root_open || root_closed {
                        bail!("gtest XML testsuites 结束标签位置非法");
                    }
                    root_open = false;
                    root_closed = true;
                }
            }
            _ => {}
        }

        match event {
            Event::Start(ref event) if event.name().as_ref() == b"testsuites" => {
                if aggregate.is_some() {
                    bail!("gtest XML 包含多个 testsuites 根节点");
                }
                let attrs = attributes(event)?;
                aggregate = Some((
                    required_usize(&attrs, "tests")?,
                    required_usize(&attrs, "failures")?,
                    required_usize(&attrs, "errors")?,
                    required_usize(&attrs, "disabled")?,
                    optional_usize(&attrs, "skipped")?,
                ));
            }
            Event::Start(ref event) if event.name().as_ref() == b"testcase" => {
                if pending.is_some() {
                    bail!("gtest XML testcase 非法嵌套");
                }
                pending = Some(begin_case(event)?);
            }
            Event::Empty(ref event) if event.name().as_ref() == b"testcase" => {
                let case = finish_case(begin_case(event)?)?;
                insert_case(&mut cases, &mut names, case)?;
            }
            Event::Start(ref event) | Event::Empty(ref event)
                if event.name().as_ref() == b"skipped" =>
            {
                pending
                    .as_mut()
                    .context("skipped 元素不在 testcase 内")?
                    .has_skipped = true;
            }
            Event::Start(ref event) | Event::Empty(ref event)
                if event.name().as_ref() == b"failure" =>
            {
                pending
                    .as_mut()
                    .context("failure 元素不在 testcase 内")?
                    .has_failure = true;
            }
            Event::Start(ref event) | Event::Empty(ref event)
                if event.name().as_ref() == b"error" =>
            {
                pending
                    .as_mut()
                    .context("error 元素不在 testcase 内")?
                    .has_error = true;
            }
            Event::End(ref event) if event.name().as_ref() == b"testcase" => {
                let case = finish_case(pending.take().context("testcase 结束标签无起始标签")?)?;
                insert_case(&mut cases, &mut names, case)?;
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    if pending.is_some() {
        bail!("gtest XML testcase 未闭合");
    }
    if depth != 0 || root_open || !root_closed {
        bail!("gtest XML testsuites 根节点未闭合");
    }
    let (total, failures, errors, disabled, root_skipped) =
        aggregate.context("缺少 testsuites 根节点")?;
    let case_skipped = cases.iter().filter(|case| case.skipped).count();
    let skipped = root_skipped.unwrap_or(case_skipped);
    let report = GtestReport {
        total,
        failures,
        errors,
        disabled,
        skipped,
        cases,
    };
    validate_aggregate(&report)?;
    Ok(report)
}

impl GtestReport {
    pub fn validate_required(
        &self,
        test_name: &str,
        expected: usize,
        allowed_skips: &HashSet<String>,
    ) -> Result<()> {
        if self.total != expected {
            bail!(
                "{} 实际执行 {} 个用例，required 清单要求 {} 个",
                test_name,
                self.total,
                expected
            );
        }
        if self.failures != 0 || self.errors != 0 || self.disabled != 0 {
            bail!(
                "{} required 门禁失败: total={}, failures={}, errors={}, disabled={}, skipped={}",
                test_name,
                self.total,
                self.failures,
                self.errors,
                self.disabled,
                self.skipped
            );
        }
        let unexpected_skips: Vec<_> = self
            .cases
            .iter()
            .filter(|case| case.skipped && !allowed_skips.contains(&case.name))
            .map(|case| case.name.as_str())
            .collect();
        if !unexpected_skips.is_empty() {
            bail!(
                "{} required 测试出现未授权 skip: {}",
                test_name,
                unexpected_skips.join(", ")
            );
        }
        Ok(())
    }
}

fn attributes(event: &BytesStart<'_>) -> Result<HashMap<String, String>> {
    let mut values = HashMap::new();
    for attr in event.attributes() {
        let attr = attr.context("读取 XML 属性失败")?;
        let key = std::str::from_utf8(attr.key.as_ref()).context("XML 属性名不是 UTF-8")?;
        let value = attr
            .unescape_value()
            .context("XML 属性值转义非法")?
            .into_owned();
        if values.insert(key.to_string(), value).is_some() {
            bail!("XML 属性重复: {}", key);
        }
    }
    Ok(values)
}

fn required_usize(attrs: &HashMap<String, String>, name: &str) -> Result<usize> {
    attrs
        .get(name)
        .with_context(|| format!("testsuites 缺少 {} 属性", name))?
        .parse::<usize>()
        .with_context(|| format!("testsuites 的 {} 不是非负整数", name))
}

fn optional_usize(attrs: &HashMap<String, String>, name: &str) -> Result<Option<usize>> {
    attrs
        .get(name)
        .map(|value| {
            value
                .parse::<usize>()
                .with_context(|| format!("testsuites 的 {} 不是非负整数", name))
        })
        .transpose()
}

fn begin_case(event: &BytesStart<'_>) -> Result<PendingCase> {
    let attrs = attributes(event)?;
    let classname = attrs.get("classname").context("testcase 缺少 classname")?;
    let name = attrs.get("name").context("testcase 缺少 name")?;
    if classname.is_empty() || name.is_empty() {
        bail!("testcase classname/name 不能为空");
    }
    Ok(PendingCase {
        name: format!("{}.{}", classname, name),
        status: attrs.get("status").context("testcase 缺少 status")?.clone(),
        result: attrs.get("result").context("testcase 缺少 result")?.clone(),
        has_skipped: false,
        has_failure: false,
        has_error: false,
    })
}

fn finish_case(case: PendingCase) -> Result<GtestCase> {
    let (skipped, disabled) = match (case.status.as_str(), case.result.as_str()) {
        ("run", "completed") if !case.has_skipped => (false, false),
        ("run", "skipped") if case.has_skipped => (true, false),
        ("notrun", "suppressed") if !case.has_skipped && !case.has_failure && !case.has_error => {
            (false, true)
        }
        _ => bail!(
            "testcase {} 状态不一致: status={}, result={}, skipped_element={}",
            case.name,
            case.status,
            case.result,
            case.has_skipped
        ),
    };
    if skipped && (case.has_failure || case.has_error) {
        bail!("testcase {} 同时包含 skip 与 failure/error", case.name);
    }
    Ok(GtestCase {
        name: case.name,
        skipped,
        failed: case.has_failure,
        error: case.has_error,
        disabled,
    })
}

fn insert_case(
    cases: &mut Vec<GtestCase>,
    names: &mut HashSet<String>,
    case: GtestCase,
) -> Result<()> {
    if !names.insert(case.name.clone()) {
        bail!("gtest XML 包含重复 testcase: {}", case.name);
    }
    cases.push(case);
    Ok(())
}

fn validate_aggregate(report: &GtestReport) -> Result<()> {
    if report.total == 0 {
        bail!("gtest XML 报告 0 个 testcase");
    }
    let failures = report.cases.iter().filter(|case| case.failed).count();
    let errors = report.cases.iter().filter(|case| case.error).count();
    let disabled = report.cases.iter().filter(|case| case.disabled).count();
    let skipped = report.cases.iter().filter(|case| case.skipped).count();
    if report.total != report.cases.len()
        || report.failures != failures
        || report.errors != errors
        || report.disabled != disabled
        || report.skipped != skipped
    {
        bail!(
            "gtest XML 汇总与 testcase 不一致: root=({},{},{},{},{}), cases=({},{},{},{},{})",
            report.total,
            report.failures,
            report.errors,
            report.disabled,
            report.skipped,
            report.cases.len(),
            failures,
            errors,
            disabled,
            skipped
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn parse(xml: &str) -> Result<GtestReport> {
        let path = std::env::temp_dir().join(format!(
            "gvisor-runner-xml-{}-{}.xml",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, xml).unwrap();
        let result = parse_gtest_xml(&path);
        let _ = fs::remove_file(path);
        result
    }

    #[test]
    fn parses_completed_and_skipped_cases() {
        let report = parse(
            r#"<?xml version="1.0"?>
          <testsuites tests="2" failures="0" disabled="0" errors="0" skipped="1">
            <testsuite name="Suite" tests="2" failures="0" disabled="0" errors="0" skipped="1">
              <testcase name="Pass" status="run" result="completed" classname="Suite" />
              <testcase name="Skip&amp;Case" status="run" result="skipped" classname="Suite">
                <skipped message="configuration" />
              </testcase>
            </testsuite>
          </testsuites>"#,
        )
        .unwrap();
        assert_eq!(report.total, 2);
        assert_eq!(report.skipped, 1);
        assert_eq!(report.cases[1].name, "Suite.Skip&Case");
    }

    #[test]
    fn derives_skipped_count_when_root_omits_it() {
        let report = parse(
            r#"<testsuites tests="1" failures="0" disabled="0" errors="0">
              <testsuite skipped="1"><testcase name="Skip" status="run" result="skipped" classname="Suite"><skipped /></testcase></testsuite>
            </testsuites>"#,
        )
        .unwrap();
        assert_eq!(report.skipped, 1);
    }

    #[test]
    fn rejects_aggregate_mismatch() {
        let error = parse(r#"<testsuites tests="2" failures="0" disabled="0" errors="0" skipped="0">
          <testsuite><testcase name="Only" status="run" result="completed" classname="Suite" /></testsuite>
        </testsuites>"#).unwrap_err();
        assert!(error.to_string().contains("汇总"));
    }

    #[test]
    fn rejects_zero_case_report() {
        assert!(parse(
            r#"<testsuites tests="0" failures="0" disabled="0" errors="0" skipped="0"></testsuites>"#,
        )
        .is_err());
    }

    #[test]
    fn rejects_truncated_document_with_complete_case_count() {
        assert!(parse(
            r#"<testsuites tests="1" failures="0" disabled="0" errors="0" skipped="0">
              <testsuite><testcase name="Only" status="run" result="completed" classname="Suite" />"#,
        )
        .is_err());
    }

    #[test]
    fn rejects_testcase_outside_testsuite() {
        assert!(parse(
            r#"<testsuites tests="1" failures="0" disabled="0" errors="0" skipped="0">
              <testcase name="Only" status="run" result="completed" classname="Suite" />
            </testsuites>"#,
        )
        .is_err());
    }

    #[test]
    fn rejects_skip_without_skip_element() {
        assert!(parse(r#"<testsuites tests="1" failures="0" disabled="0" errors="0" skipped="1">
          <testsuite><testcase name="Bad" status="run" result="skipped" classname="Suite" /></testsuite>
        </testsuites>"#).is_err());
    }

    #[test]
    fn required_report_rejects_partial_or_skipped_run() {
        let report = parse(r#"<testsuites tests="1" failures="0" disabled="0" errors="0" skipped="1">
          <testsuite><testcase name="Skip" status="run" result="skipped" classname="Suite"><skipped /></testcase></testsuite>
        </testsuites>"#).unwrap();
        let none = HashSet::new();
        assert!(report.validate_required("binary", 2, &none).is_err());
        assert!(report.validate_required("binary", 1, &none).is_err());
        let allowed = HashSet::from(["Suite.Skip".to_string()]);
        assert!(report.validate_required("binary", 1, &allowed).is_ok());
    }
}
