// -*- coding: utf-8 -*-
// ------------------------------------------------------------------------------------------------
// Copyright © 2021, stack-graphs authors.
// Licensed under either of Apache License, Version 2.0, or MIT license, at your option.
// Please see the LICENSE-APACHE or LICENSE-MIT files in this distribution for license details.
// ------------------------------------------------------------------------------------------------

//! Defines loader for stack graph languages
//!
//! The loader is created from a tree-sitter configuration or list of search paths, an optional scope,
//! and a function that may load the tree-sitter-graph file for a language.
//!
//! The loader is called with a file path and optional file content and tries to find the language for
//! that file. The loader will search for tree-sitter languages in the given search paths, or in current
//! directory and the paths defined in the tree-sitter configuration. If a scope is provided, it will be
//! used to restrict the discovered languages to those with a matching scope. If no languages were found
//! at all, an error is raised. Otherwise, a language matching the file path and content is returned, if
//! it exists among the discovered languages.
//!
//! Previously loaded languages are cached in the loader, so subsequent loads are fast.

use ini::Ini;
use itertools::Itertools;
use regex::Regex;
use stack_graphs::graph::StackGraph;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::Path;
use std::path::PathBuf;
use thiserror::Error;
use tree_sitter::Language;
use tree_sitter_graph::ast::File as TsgFile;
use tree_sitter_graph::Variables;
use tree_sitter_loader::Config as TsConfig;
use tree_sitter_loader::LanguageConfiguration;
use tree_sitter_loader::Loader as TsLoader;

use crate::CancellationFlag;
use crate::StackGraphLanguage;

pub trait Reader {
    fn read(
        &self,
        language: Language,
    ) -> Result<Option<TsgFile>, Box<dyn std::error::Error + Send + Sync>>;
}

impl<F> Reader for F
where
    F: Fn(Language) -> Result<Option<TsgFile>, Box<dyn std::error::Error + Send + Sync>>,
{
    fn read(
        &self,
        language: Language,
    ) -> Result<Option<TsgFile>, Box<dyn std::error::Error + Send + Sync>> {
        self(language)
    }
}

pub struct Loader {
    loader: SupplementedTsLoader,
    paths: Vec<PathBuf>,
    scope: Option<String>,
    tsg: Box<dyn Reader>,
    cache: Vec<(Language, StackGraphLanguage)>,
}

impl Loader {
    pub fn from_paths(
        paths: Vec<PathBuf>,
        scope: Option<String>,
        tsg: impl Reader + 'static,
    ) -> Result<Self, LoadError> {
        Ok(Self {
            loader: SupplementedTsLoader::new()?,
            paths,
            scope,
            tsg: Box::new(tsg),
            cache: Vec::new(),
        })
    }

    pub fn from_config(
        config: &TsConfig,
        scope: Option<String>,
        tsg: impl Reader + 'static,
    ) -> Result<Self, LoadError> {
        Ok(Self {
            loader: SupplementedTsLoader::new()?,
            paths: Self::config_paths(config)?,
            scope,
            tsg: Box::new(tsg),
            cache: Vec::new(),
        })
    }

    // Adopted from tree_sitter_loader::Loader::load
    fn config_paths(config: &TsConfig) -> Result<Vec<PathBuf>, LoadError> {
        if config.parser_directories.is_empty() {
            eprintln!("Warning: You have not configured any parser directories!");
            eprintln!("Please run `tree-sitter init-config` and edit the resulting");
            eprintln!("configuration file to indicate where we should look for");
            eprintln!("language grammars.");
            eprintln!("");
        }
        let mut paths = Vec::new();
        for parser_container_dir in &config.parser_directories {
            if let Ok(entries) = std::fs::read_dir(parser_container_dir) {
                for entry in entries {
                    let entry = entry?;
                    if let Some(parser_dir_name) = entry.file_name().to_str() {
                        if parser_dir_name.starts_with("tree-sitter-") {
                            paths.push(parser_container_dir.join(parser_dir_name));
                        }
                    }
                }
            }
        }
        Ok(paths)
    }

    pub fn load_for_file(
        &mut self,
        path: &Path,
        content: Option<&str>,
        cancellation_flag: &dyn CancellationFlag,
    ) -> Result<Option<&mut StackGraphLanguage>, LoadError> {
        let selected_language = self.select_language_for_file(path, content)?;
        let language = match selected_language {
            Some(selected_language) => selected_language.clone(),
            None => return Ok(None),
        };
        // the borrow checker is a hard master...
        let index = self.cache.iter().position(|e| &e.0 == &language.language);
        let index = match index {
            Some(index) => index,
            None => {
                let tsg = self.load_tsg_for_language(&language)?;
                let mut sgl = StackGraphLanguage::new(language.language, tsg)?;
                self.load_builtins(&language, &mut sgl, cancellation_flag)?;
                self.cache.push((language.language, sgl));

                self.cache.len() - 1
            }
        };
        let sgl = &mut self.cache[index].1;
        Ok(Some(sgl))
    }

    // Select language for the given file, considering paths and scope fields
    fn select_language_for_file(
        &mut self,
        file_path: &Path,
        file_content: Option<&str>,
    ) -> Result<Option<&SupplementedLanguage>, LoadError> {
        // The borrow checker is not smart enough to realize that the early returns
        // ensure any references from the self.select_* call (which require a mutable
        // borrow) do not outlive the match. Therefore, we use a raw self_ptr and unsafe
        // dereferencing to make those calls.
        let self_ptr = self as *mut Self;
        let mut found_languages = false;
        for path in &self.paths {
            found_languages |= match unsafe { &mut *self_ptr }.select_language_for_file_from_path(
                &path,
                file_path,
                file_content,
            ) {
                Ok(Some(language)) => return Ok(Some(language)),
                Ok(None) => true,
                Err(LoadError::NoLanguagesFound(_)) => false,
                Err(err) => return Err(err),
            };
        }
        if !found_languages {
            return Err(LoadError::NoLanguagesFound(format!(
                "in {}{}",
                self.paths.iter().map(|p| p.display()).format(":"),
                self.scope
                    .as_ref()
                    .map_or(String::default(), |s| format!(" for scope {}", s)),
            )));
        }
        Ok(None)
    }

    // Select language from the given path for the given file, considering scope field
    fn select_language_for_file_from_path(
        &mut self,
        language_path: &Path,
        file_path: &Path,
        file_content: Option<&str>,
    ) -> Result<Option<&SupplementedLanguage>, LoadError> {
        let scope = self.scope.as_deref();
        let languages = self.loader.languages_at_path(language_path, scope)?;
        if languages.is_empty() {
            return Err(LoadError::NoLanguagesFound(format!(
                "at {}{}",
                language_path.display(),
                scope.map_or(String::default(), |s| format!(" for scope {}", s)),
            )));
        }
        if let Some(language) =
            SupplementedLanguage::best_for_file(languages, file_path, file_content)
        {
            return Ok(Some(language));
        };
        Ok(None)
    }

    // Load the TSG file for the given language and path
    fn load_tsg_for_language(&self, language: &SupplementedLanguage) -> Result<TsgFile, LoadError> {
        if let Some(tsg) = self
            .tsg
            .read(language.language)
            .map_err(LoadError::Reader)?
        {
            return Ok(tsg);
        }

        let tsg_path = language.root_path.join("queries/stack-graphs.tsg");
        if tsg_path.exists() {
            let tsg_source = std::fs::read_to_string(tsg_path.clone())?;
            let tsg = TsgFile::from_str(language.language, &tsg_source)?;
            return Ok(tsg);
        }

        return Err(LoadError::NoTsgFound);
    }

    // Builtins are loaded from queries/builtins.EXT and an optional queries/builtins.cfg configuration.
    // In the future, we may extend this to support builtins spread over multiple files queries/builtins/NAME.EXT
    // and optional corresponding configuration files queries/builtins/NAME.cfg.
    fn load_builtins(
        &self,
        language: &SupplementedLanguage,
        sgl: &mut StackGraphLanguage,
        cancellation_flag: &dyn CancellationFlag,
    ) -> Result<(), LoadError> {
        let mut graph = StackGraph::new();
        for ext in &language.file_types {
            let path = language.root_path.join(format!("queries/builtins.{}", ext));
            if path.exists() {
                let file = graph.add_file(&path.to_string_lossy()).unwrap();
                let source = std::fs::read_to_string(path.clone())?;
                let mut globals = Variables::new();
                let globals_path = language.root_path.join("queries/builtins.cfg");
                if globals_path.exists() {
                    self.load_config_from_path(&globals_path, &mut globals)?;
                }
                sgl.build_stack_graph_into(&mut graph, file, &source, &globals, cancellation_flag)?;
            }
        }
        sgl.builtins_mut().add_from_graph(&graph).unwrap();
        Ok(())
    }

    pub fn load_config_from_path(
        &self,
        path: &Path,
        globals: &mut Variables,
    ) -> Result<(), LoadError> {
        let conf = Ini::load_from_file(path)?;
        self.load_config(&conf, globals)
    }

    pub fn load_config_from_str(
        &self,
        config: &str,
        globals: &mut Variables,
    ) -> Result<(), LoadError> {
        let conf = Ini::load_from_str(config).map_err(ini::Error::Parse)?;
        self.load_config(&conf, globals)
    }

    fn load_config(&self, conf: &Ini, globals: &mut Variables) -> Result<(), LoadError> {
        if let Some(globals_section) = conf.section(Some("globals")) {
            for (name, value) in globals_section.iter() {
                globals.add(name.into(), value.into()).map_err(|_| {
                    LoadError::Reader(
                        format!("Duplicate global variable {} in config", name).into(),
                    )
                })?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum LoadError {
    #[error("{0}")]
    Cancelled(&'static str),
    #[error(transparent)]
    Config(#[from] ini::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Language(#[from] crate::LanguageError),
    #[error("No languages found {0}")]
    NoLanguagesFound(String),
    #[error("No TSG file found")]
    NoTsgFound,
    #[error(transparent)]
    Reader(Box<dyn std::error::Error + Send + Sync>),
    #[error(transparent)]
    StackGraph(crate::LoadError),
    #[error(transparent)]
    TsgParse(#[from] tree_sitter_graph::ParseError),
    #[error(transparent)]
    TreeSitter(anyhow::Error),
}

impl From<crate::LoadError> for LoadError {
    fn from(value: crate::LoadError) -> Self {
        match value {
            crate::LoadError::Cancelled(at) => Self::Cancelled(at),
            other => Self::StackGraph(other),
        }
    }
}

// ------------------------------------------------------------------------------------------------
// tree_sitter_loader supplements

// Wraps a tree_sitter_loader::Loader
struct SupplementedTsLoader(TsLoader, HashMap<PathBuf, Vec<SupplementedLanguage>>);

impl SupplementedTsLoader {
    pub fn new() -> Result<Self, LoadError> {
        let loader = TsLoader::new().map_err(LoadError::TreeSitter)?;
        Ok(Self(loader, HashMap::new()))
    }

    pub fn languages_at_path(
        &mut self,
        path: &Path,
        scope: Option<&str>,
    ) -> Result<Vec<&SupplementedLanguage>, LoadError> {
        if !self.1.contains_key(path) {
            let languages = self
                .0
                .languages_at_path(&path)
                .map_err(LoadError::TreeSitter)?;
            let configurations = self
                .0
                .find_language_configurations_at_path(&path)
                .map_err(LoadError::TreeSitter)?;
            let languages = languages
                .into_iter()
                .zip(configurations.into_iter())
                .map(SupplementedLanguage::from)
                .filter(|language| scope.map_or(true, |scope| language.matches_scope(scope)))
                .collect::<Vec<_>>();
            self.1.insert(path.to_path_buf(), languages);
        }
        Ok(self.1[path].iter().map(|l| l).collect())
    }
}

#[derive(Clone, Debug)]
struct SupplementedLanguage {
    pub language: Language,
    pub scope: Option<String>,
    pub content_regex: Option<Regex>,
    pub file_types: Vec<String>,
    pub root_path: PathBuf,
}

impl SupplementedLanguage {
    pub fn matches_scope(&self, scope: &str) -> bool {
        self.scope.as_ref().map_or(false, |s| s == scope)
    }

    // Extracted from tree_sitter_loader::Loader::language_configuration_for_file_name
    pub fn matches_file(&self, path: &Path, content: Option<&str>) -> Option<isize> {
        // Check path extension
        if !path
            .extension()
            .and_then(OsStr::to_str)
            .map_or(false, |ext| self.file_types.iter().any(|ft| ft == ext))
        {
            return None;
        }

        // Apply content regex
        if let (Some(file_content), Some(content_regex)) = (content, &self.content_regex) {
            // If the language configuration has a content regex, assign
            // a score based on the length of the first match.
            if let Some(mat) = content_regex.find(&file_content) {
                let score = (mat.end() - mat.start()) as isize;
                return Some(score);
            } else {
                return None;
            }
        }

        Some(0isize)
    }

    // Extracted from tree_sitter_loader::Loader::language_configuration_for_file_name
    pub fn best_for_file<'a>(
        languages: Vec<&'a SupplementedLanguage>,
        path: &Path,
        content: Option<&str>,
    ) -> Option<&'a SupplementedLanguage> {
        let mut best_score = -1isize;
        let mut best = None;
        for language in languages {
            if let Some(score) = language.matches_file(path, content) {
                if score > best_score {
                    best_score = score;
                    best = Some(language);
                }
            }
        }
        best
    }
}

impl From<(Language, &LanguageConfiguration<'_>)> for SupplementedLanguage {
    fn from((language, config): (Language, &LanguageConfiguration)) -> Self {
        Self {
            scope: config.scope.clone(),
            content_regex: config.content_regex.clone(),
            file_types: config.file_types.clone(),
            root_path: config.root_path.clone(),
            language,
        }
    }
}
