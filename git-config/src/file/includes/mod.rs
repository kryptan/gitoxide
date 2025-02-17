use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};

use bstr::{BStr, BString, ByteSlice, ByteVec};
use git_features::threading::OwnShared;
use git_ref::Category;

use crate::{
    file,
    file::{includes, init, Metadata, SectionId},
    path, File,
};

impl File<'static> {
    /// Traverse all `include` and `includeIf` directives found in this instance and follow them, loading the
    /// referenced files from their location and adding their content right past the value that included them.
    ///
    /// # Limitations
    ///
    /// - Note that this method is _not idempotent_ and calling it multiple times will resolve includes multiple
    ///   times. It's recommended use is as part of a multi-step bootstrapping which needs fine-grained control,
    ///   and unless that's given one should prefer one of the other ways of initialization that resolve includes
    ///   at the right time.
    /// - included values are added after the _section_ that included them, not directly after the value. This is
    ///   a deviation from how git does it, as it technically adds new value right after the include path itself,
    ///   technically 'splitting' the section. This can only make a difference if the `include` section also has values
    ///   which later overwrite portions of the included file, which seems unusual as these would be related to `includes`.
    ///   We can fix this by 'splitting' the include section if needed so the included sections are put into the right place.
    pub fn resolve_includes(&mut self, options: init::Options<'_>) -> Result<(), Error> {
        if options.includes.max_depth == 0 {
            return Ok(());
        }
        let mut buf = Vec::new();
        resolve(self, &mut buf, options)
    }
}

pub(crate) fn resolve(config: &mut File<'static>, buf: &mut Vec<u8>, options: init::Options<'_>) -> Result<(), Error> {
    resolve_includes_recursive(config, 0, buf, options)
}

fn resolve_includes_recursive(
    target_config: &mut File<'static>,
    depth: u8,
    buf: &mut Vec<u8>,
    options: init::Options<'_>,
) -> Result<(), Error> {
    if depth == options.includes.max_depth {
        return if options.includes.err_on_max_depth_exceeded {
            Err(Error::IncludeDepthExceeded {
                max_depth: options.includes.max_depth,
            })
        } else {
            Ok(())
        };
    }

    let mut section_ids_and_include_paths = Vec::new();
    for (id, section) in target_config
        .section_order
        .iter()
        .map(|id| (*id, &target_config.sections[id]))
    {
        let header = &section.header;
        let header_name = header.name.as_ref();
        if header_name == "include" && header.subsection_name.is_none() {
            detach_include_paths(&mut section_ids_and_include_paths, section, id)
        } else if header_name == "includeIf" {
            if let Some(condition) = &header.subsection_name {
                let target_config_path = section.meta.path.as_deref();
                if include_condition_match(condition.as_ref(), target_config_path, options.includes)? {
                    detach_include_paths(&mut section_ids_and_include_paths, section, id)
                }
            }
        }
    }

    append_followed_includes_recursively(section_ids_and_include_paths, target_config, depth, options, buf)
}

fn append_followed_includes_recursively(
    section_ids_and_include_paths: Vec<(SectionId, crate::Path<'_>)>,
    target_config: &mut File<'static>,
    depth: u8,
    options: init::Options<'_>,
    buf: &mut Vec<u8>,
) -> Result<(), Error> {
    for (section_id, config_path) in section_ids_and_include_paths {
        let meta = OwnShared::clone(&target_config.sections[&section_id].meta);
        let target_config_path = meta.path.as_deref();
        let config_path = match resolve_path(config_path, target_config_path, options.includes)? {
            Some(p) => p,
            None => continue,
        };
        if !config_path.is_file() {
            continue;
        }

        buf.clear();
        std::io::copy(&mut std::fs::File::open(&config_path)?, buf)?;
        let config_meta = Metadata {
            path: Some(config_path),
            trust: meta.trust,
            level: meta.level + 1,
            source: meta.source,
        };
        let no_follow_options = init::Options {
            includes: includes::Options::no_follow(),
            ..options
        };

        let mut include_config =
            File::from_bytes_owned(buf, config_meta, no_follow_options).map_err(|err| match err {
                init::Error::Parse(err) => Error::Parse(err),
                init::Error::Interpolate(err) => Error::Interpolate(err),
                init::Error::Includes(_) => unreachable!("BUG: {:?} not possible due to no-follow options", err),
            })?;
        resolve_includes_recursive(&mut include_config, depth + 1, buf, options)?;

        target_config.append_or_insert(include_config, Some(section_id));
    }
    Ok(())
}

fn detach_include_paths(
    include_paths: &mut Vec<(SectionId, crate::Path<'static>)>,
    section: &file::Section<'_>,
    id: SectionId,
) {
    include_paths.extend(
        section
            .body
            .values("path")
            .into_iter()
            .map(|path| (id, crate::Path::from(Cow::Owned(path.into_owned())))),
    )
}

fn include_condition_match(
    condition: &BStr,
    target_config_path: Option<&Path>,
    options: Options<'_>,
) -> Result<bool, Error> {
    let mut tokens = condition.splitn(2, |b| *b == b':');
    let (prefix, condition) = match (tokens.next(), tokens.next()) {
        (Some(a), Some(b)) => (a, b),
        _ => return Ok(false),
    };
    let condition = condition.as_bstr();
    match prefix {
        b"gitdir" => gitdir_matches(
            condition,
            target_config_path,
            options,
            git_glob::wildmatch::Mode::empty(),
        ),
        b"gitdir/i" => gitdir_matches(
            condition,
            target_config_path,
            options,
            git_glob::wildmatch::Mode::IGNORE_CASE,
        ),
        b"onbranch" => Ok(onbranch_matches(condition, options.conditional).is_some()),
        _ => Ok(false),
    }
}

fn onbranch_matches(
    condition: &BStr,
    conditional::Context { branch_name, .. }: conditional::Context<'_>,
) -> Option<()> {
    let branch_name = branch_name?;
    let (_, branch_name) = branch_name
        .category_and_short_name()
        .filter(|(cat, _)| *cat == Category::LocalBranch)?;

    let condition = if condition.ends_with(b"/") {
        let mut condition: BString = condition.into();
        condition.push_str("**");
        Cow::Owned(condition)
    } else {
        condition.into()
    };

    git_glob::wildmatch(
        condition.as_ref(),
        branch_name,
        git_glob::wildmatch::Mode::NO_MATCH_SLASH_LITERAL,
    )
    .then(|| ())
}

fn gitdir_matches(
    condition_path: &BStr,
    target_config_path: Option<&Path>,
    Options {
        conditional: conditional::Context { git_dir, .. },
        interpolate: context,
        err_on_interpolation_failure,
        err_on_missing_config_path,
        ..
    }: Options<'_>,
    wildmatch_mode: git_glob::wildmatch::Mode,
) -> Result<bool, Error> {
    if !err_on_interpolation_failure && git_dir.is_none() {
        return Ok(false);
    }
    let git_dir = git_path::to_unix_separators_on_windows(git_path::into_bstr(git_dir.ok_or(Error::MissingGitDir)?));

    let mut pattern_path: Cow<'_, _> = {
        let path = match check_interpolation_result(
            err_on_interpolation_failure,
            crate::Path::from(Cow::Borrowed(condition_path)).interpolate(context),
        )? {
            Some(p) => p,
            None => return Ok(false),
        };
        git_path::into_bstr(path).into_owned().into()
    };
    // NOTE: yes, only if we do path interpolation will the slashes be forced to unix separators on windows
    if pattern_path != condition_path {
        pattern_path = git_path::to_unix_separators_on_windows(pattern_path);
    }

    if let Some(relative_pattern_path) = pattern_path.strip_prefix(b"./") {
        if !err_on_missing_config_path && target_config_path.is_none() {
            return Ok(false);
        }
        let parent_dir = target_config_path
            .ok_or(Error::MissingConfigPath)?
            .parent()
            .expect("config path can never be /");
        let mut joined_path = git_path::to_unix_separators_on_windows(git_path::into_bstr(parent_dir)).into_owned();
        joined_path.push(b'/');
        joined_path.extend_from_slice(relative_pattern_path);
        pattern_path = joined_path.into();
    }

    // NOTE: this special handling of leading backslash is needed to do it like git does
    if pattern_path.iter().next() != Some(&(std::path::MAIN_SEPARATOR as u8))
        && !git_path::from_bstr(pattern_path.clone()).is_absolute()
    {
        let mut prefixed = pattern_path.into_owned();
        prefixed.insert_str(0, "**/");
        pattern_path = prefixed.into()
    }
    if pattern_path.ends_with(b"/") {
        let mut suffixed = pattern_path.into_owned();
        suffixed.push_str("**");
        pattern_path = suffixed.into();
    }

    let match_mode = git_glob::wildmatch::Mode::NO_MATCH_SLASH_LITERAL | wildmatch_mode;
    let is_match = git_glob::wildmatch(pattern_path.as_bstr(), git_dir.as_bstr(), match_mode);
    if is_match {
        return Ok(true);
    }

    let expanded_git_dir = git_path::into_bstr(git_path::realpath(git_path::from_byte_slice(&git_dir))?);
    Ok(git_glob::wildmatch(
        pattern_path.as_bstr(),
        expanded_git_dir.as_bstr(),
        match_mode,
    ))
}

fn check_interpolation_result(
    disable: bool,
    res: Result<Cow<'_, std::path::Path>, path::interpolate::Error>,
) -> Result<Option<Cow<'_, std::path::Path>>, path::interpolate::Error> {
    if disable {
        return res.map(Some);
    }
    match res {
        Ok(good) => Ok(good.into()),
        Err(err) => match err {
            path::interpolate::Error::Missing { .. } | path::interpolate::Error::UserInterpolationUnsupported => {
                Ok(None)
            }
            path::interpolate::Error::UsernameConversion(_) | path::interpolate::Error::Utf8Conversion { .. } => {
                Err(err)
            }
        },
    }
}

fn resolve_path(
    path: crate::Path<'_>,
    target_config_path: Option<&Path>,
    includes::Options {
        interpolate: context,
        err_on_interpolation_failure,
        err_on_missing_config_path,
        ..
    }: includes::Options<'_>,
) -> Result<Option<PathBuf>, Error> {
    let path = match check_interpolation_result(err_on_interpolation_failure, path.interpolate(context))? {
        Some(p) => p,
        None => return Ok(None),
    };
    let path: PathBuf = if path.is_relative() {
        if !err_on_missing_config_path && target_config_path.is_none() {
            return Ok(None);
        }
        target_config_path
            .ok_or(Error::MissingConfigPath)?
            .parent()
            .expect("path is a config file which naturally lives in a directory")
            .join(path)
    } else {
        path.into()
    };
    Ok(Some(path))
}

mod types;
pub use types::{conditional, Error, Options};
