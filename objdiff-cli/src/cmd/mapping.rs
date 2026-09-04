use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, bail};
use argp::FromArgs;
use objdiff_core::{
    config::{
        ProjectConfig, ProjectConfigInfo, ProjectOptions, apply_project_options,
        path::{check_path_buf, platform_path},
        save_project_config, try_project_config,
    },
    diff::{self, DiffObjConfig, DiffSide, MappingConfig},
    obj::{self, Object, SectionKind, Symbol, SymbolFlag},
};
use serde::Serialize;
use typed_path::Utf8PlatformPathBuf;

use crate::{
    cmd::apply_config_args,
    util::output::{OutputFormat, write_json_output},
};

#[derive(FromArgs, PartialEq, Debug)]
/// Show possible symbol mappings for a symbol in a project unit.
#[argp(subcommand, name = "map")]
pub struct MapArgs {
    #[argp(option, short = 'p', from_str_fn(platform_path))]
    /// Project directory (default: current directory)
    project: Option<Utf8PlatformPathBuf>,
    #[argp(option, short = 'u')]
    /// Unit name within the project
    unit: String,
    #[argp(option)]
    /// Target symbol whose possible base mappings should be shown
    target: Option<String>,
    #[argp(option)]
    /// Base symbol whose possible target mappings should be shown
    base: Option<String>,
    #[argp(option, short = 'o', from_str_fn(platform_path))]
    /// Output file ("-" for stdout)
    output: Option<Utf8PlatformPathBuf>,
    #[argp(option)]
    /// Output format (json, json-pretty) (default: json)
    format: Option<String>,
    #[argp(switch)]
    /// Include candidates that are already paired
    show_mapped: bool,
    #[argp(option, short = 'c')]
    /// Configuration property (key=value)
    config: Vec<String>,
}

#[derive(FromArgs, PartialEq, Debug)]
/// Pair two symbols and save the mapping in the project configuration.
#[argp(subcommand, name = "pair")]
pub struct PairArgs {
    #[argp(option, short = 'p', from_str_fn(platform_path))]
    /// Project directory (default: current directory)
    project: Option<Utf8PlatformPathBuf>,
    #[argp(option, short = 'u')]
    /// Unit name within the project
    unit: String,
    #[argp(option)]
    /// Target symbol name
    target: String,
    #[argp(option)]
    /// Base symbol name
    base: String,
    #[argp(option, short = 'c')]
    /// Configuration property (key=value)
    config: Vec<String>,
}

#[derive(Clone)]
struct UnitContext {
    project: Utf8PlatformPathBuf,
    config: ProjectConfig,
    config_info: ProjectConfigInfo,
    unit_index: usize,
    unit_name: String,
    target_path: Utf8PlatformPathBuf,
    base_path: Utf8PlatformPathBuf,
    unit_options: Option<ProjectOptions>,
    symbol_mappings: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct MappingOutput {
    project: String,
    unit: String,
    source_side: &'static str,
    source_symbol: String,
    candidates: Vec<MappingCandidate>,
}

#[derive(Serialize)]
struct MappingCandidate {
    target: String,
    base: String,
    demangled_name: Option<String>,
    match_percent: Option<f32>,
}

pub fn map(args: MapArgs) -> Result<()> {
    let source_symbol = match (args.target.as_deref(), args.base.as_deref()) {
        (Some(source), None) | (None, Some(source)) => source,
        (None, None) => bail!("Specify exactly one of --target or --base"),
        (Some(_), Some(_)) => bail!("Specify exactly one of --target or --base"),
    };
    let source_side = if args.target.is_some() { "target" } else { "base" };

    let unit = load_unit(args.project, &args.unit)?;
    let diff_config = build_diff_config(&unit, &args.config)?;
    let (target, base) = read_unit_objects(&unit, &diff_config)?;
    let source_obj = if source_side == "target" { &target } else { &base };
    let source_symbol_index = source_obj
        .symbol_by_name(source_symbol)
        .ok_or_else(|| anyhow!("{} symbol not found: {}", source_side, source_symbol))?;
    validate_mapping_source(source_obj, source_symbol_index, source_side, source_symbol)?;

    let mapping_config = MappingConfig {
        mappings: unit.symbol_mappings.clone(),
        selecting_left: (source_side == "base").then(|| source_symbol.to_string()),
        selecting_right: (source_side == "target").then(|| source_symbol.to_string()),
    };
    let result = diff::diff_objs(Some(&target), Some(&base), None, &diff_config, &mapping_config)?;
    let candidate_obj_diff = if source_side == "target" {
        result.right.as_ref().unwrap()
    } else {
        result.left.as_ref().unwrap()
    };
    let candidate_obj = if source_side == "target" { &base } else { &target };
    let candidates = candidate_obj_diff
        .mapping_symbols
        .iter()
        .filter(|candidate| {
            args.show_mapped
                || candidate_obj_diff.symbols[candidate.symbol_index].target_symbol.is_none()
        })
        .map(|candidate| {
            let symbol = &candidate_obj.symbols[candidate.symbol_index];
            let (target, base) = if source_side == "target" {
                (source_symbol.to_string(), symbol.name.clone())
            } else {
                (symbol.name.clone(), source_symbol.to_string())
            };
            MappingCandidate {
                target,
                base,
                demangled_name: symbol.demangled_name.clone(),
                match_percent: candidate.symbol_diff.match_percent,
            }
        })
        .collect();

    let pretty = parse_json_format(args.format.as_deref())?;
    write_json_output(
        &MappingOutput {
            project: unit.project.to_string(),
            unit: unit.unit_name,
            source_side,
            source_symbol: source_symbol.to_string(),
            candidates,
        },
        args.output.as_deref(),
        pretty,
    )
}

pub fn pair(args: PairArgs) -> Result<()> {
    let unit = load_unit(args.project, &args.unit)?;
    let diff_config = build_diff_config(&unit, &args.config)?;
    let (target, base) = read_unit_objects(&unit, &diff_config)?;
    let target_symbol_index = target
        .symbol_by_name(&args.target)
        .ok_or_else(|| anyhow!("Target symbol not found: {}", args.target))?;
    let base_symbol_index = base
        .symbol_by_name(&args.base)
        .ok_or_else(|| anyhow!("Base symbol not found: {}", args.base))?;
    validate_pair(
        &target,
        target_symbol_index,
        &base,
        base_symbol_index,
        &args.target,
        &args.base,
    )?;

    let mut config = unit.config;
    let project_info = unit.config_info;
    let project = unit.project;
    let units = config.units.as_mut().unwrap();
    let project_unit = units.get_mut(unit.unit_index).unwrap();
    let mappings = project_unit.symbol_mappings.get_or_insert_with(BTreeMap::new);
    mappings.retain(|target, base| target != &args.target && base != &args.base);
    if args.target != args.base {
        mappings.insert(args.target.clone(), args.base.clone());
    }
    save_project_config(&config, &project_info)?;

    if args.target == args.base {
        println!(
            "Removed explicit mapping for {} in unit {} (symbols with the same name pair automatically)",
            args.target, unit.unit_name
        );
    } else {
        println!(
            "Paired target '{}' with base '{}' in unit {} ({})",
            args.target, args.base, unit.unit_name, project
        );
    }
    Ok(())
}

fn load_unit(project_arg: Option<Utf8PlatformPathBuf>, unit_name: &str) -> Result<UnitContext> {
    let project = match project_arg {
        Some(project) => project,
        None => check_path_buf(std::env::current_dir().context("Failed to get current directory")?)
            .context("Current directory is not valid UTF-8")?,
    };
    let Some((config_result, config_info)) = try_project_config(project.as_ref()) else {
        bail!("Project config not found in {}", project);
    };
    let config = config_result
        .with_context(|| format!("Reading project config {}", config_info.path.display()))?;
    let units = config.units.as_deref().unwrap_or_default();
    let unit_index = units
        .iter()
        .position(|unit| unit.name() == unit_name)
        .ok_or_else(|| anyhow!("Unit not found: {}", unit_name))?;
    let target_obj_dir =
        config.target_dir.as_ref().map(|path| project.join(path.with_platform_encoding()));
    let base_obj_dir =
        config.base_dir.as_ref().map(|path| project.join(path.with_platform_encoding()));
    let object = crate::cmd::diff::ObjectConfig::new(
        &units[unit_index],
        &project,
        target_obj_dir.as_deref(),
        base_obj_dir.as_deref(),
    );
    let target_path =
        object.target_path.ok_or_else(|| anyhow!("Unit {} has no target object", unit_name))?;
    let base_path =
        object.base_path.ok_or_else(|| anyhow!("Unit {} has no base object", unit_name))?;
    let unit_options = units[unit_index].options().cloned();
    let symbol_mappings = object.symbol_mappings;
    Ok(UnitContext {
        project,
        config,
        config_info,
        unit_index,
        unit_name: unit_name.to_string(),
        target_path,
        base_path,
        unit_options,
        symbol_mappings,
    })
}

fn build_diff_config(unit: &UnitContext, config_args: &[String]) -> Result<DiffObjConfig> {
    let mut diff_config = DiffObjConfig::default();
    if let Some(options) = unit.config.options.as_ref() {
        apply_project_options(&mut diff_config, options)?;
    }
    if let Some(options) = unit.unit_options.as_ref() {
        apply_project_options(&mut diff_config, options)?;
    }
    apply_config_args(&mut diff_config, config_args)?;
    Ok(diff_config)
}

fn read_unit_objects(unit: &UnitContext, diff_config: &DiffObjConfig) -> Result<(Object, Object)> {
    let target = obj::read::read(unit.target_path.as_ref(), diff_config, DiffSide::Target)
        .with_context(|| format!("Loading {}", unit.target_path))?;
    let base = obj::read::read(unit.base_path.as_ref(), diff_config, DiffSide::Base)
        .with_context(|| format!("Loading {}", unit.base_path))?;
    Ok((target, base))
}

fn parse_json_format(format: Option<&str>) -> Result<bool> {
    match OutputFormat::from_option(format)? {
        OutputFormat::Json => Ok(false),
        OutputFormat::JsonPretty => Ok(true),
        OutputFormat::Proto => bail!("The map command only supports json and json-pretty output"),
    }
}

fn validate_mapping_source(
    obj: &Object,
    symbol_index: usize,
    side: &str,
    name: &str,
) -> Result<()> {
    let symbol = &obj.symbols[symbol_index];
    if symbol.section.is_none() {
        bail!("{} symbol has no section and cannot be mapped: {}", side, name);
    }
    if symbol.size == 0 || symbol.flags.contains(SymbolFlag::Ignored) {
        bail!("{} symbol cannot be mapped: {}", side, name);
    }
    Ok(())
}

fn validate_pair(
    target: &Object,
    target_index: usize,
    base: &Object,
    base_index: usize,
    target_name: &str,
    base_name: &str,
) -> Result<()> {
    validate_mapping_source(target, target_index, "Target", target_name)?;
    validate_mapping_source(base, base_index, "Base", base_name)?;
    let target_kind = symbol_section_kind(target, &target.symbols[target_index]);
    let base_kind = symbol_section_kind(base, &base.symbols[base_index]);
    if target_kind != base_kind {
        bail!(
            "Cannot pair symbols from different section kinds: {} ({target_kind:?}) vs {} ({base_kind:?})",
            target_name,
            base_name
        );
    }
    Ok(())
}

fn symbol_section_kind(obj: &Object, symbol: &Symbol) -> SectionKind {
    match symbol.section {
        Some(section_index) => obj.sections[section_index].kind,
        None if symbol.flags.contains(SymbolFlag::Common) => SectionKind::Common,
        None => SectionKind::Unknown,
    }
}
