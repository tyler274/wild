use super::types::*;
use crate::bail;
use crate::ensure;
use crate::error::Context;
use crate::error::Result;
use crate::platform;
use hashbrown::HashMap;
use hashbrown::HashSet;
use itertools::Itertools;
use std::borrow::Cow;
use std::path::Path;

/// Describes how a platform spells its options. GNU-style platforms use the default, whereas
/// link.exe-style platforms accept a `/` prefix, ignore case and attach values with `:`.
#[derive(Clone, Copy)]
pub(crate) struct OptionSyntax {
    /// Prefixes that introduce an option. Matched in order, so longer prefixes must come first.
    pub(crate) prefixes: &'static [&'static str],

    /// The character that attaches a value to an option name, e.g. `=` in `--foo=bar`.
    pub(crate) value_separator: char,

    pub(crate) case_insensitive: bool,

    /// Whether an option that takes a value may instead take it from the following token. When
    /// false, as for link.exe, the value must be attached to the option name, so a missing value
    /// is an error, as is a value supplied to an option that doesn't take one.
    pub(crate) allows_separate_value: bool,
}

impl Default for OptionSyntax {
    fn default() -> Self {
        Self {
            prefixes: &["--", "-"],
            value_separator: '=',
            case_insensitive: false,
            allows_separate_value: true,
        }
    }
}

impl OptionSyntax {
    /// Strips the first matching option prefix from `arg`, returning the rest of the argument.
    fn strip_prefix<'a>(&self, arg: &'a str) -> Option<&'a str> {
        self.prefixes
            .iter()
            .find_map(|prefix| arg.strip_prefix(prefix))
    }

    fn starts_with_prefix(&self, arg: &str) -> bool {
        self.strip_prefix(arg).is_some()
    }

    /// Returns the key under which an option with this name should be stored and looked up.
    fn lookup_key<'a>(&self, name: &'a str) -> Cow<'a, str> {
        if self.case_insensitive {
            Cow::Owned(name.to_ascii_lowercase())
        } else {
            Cow::Borrowed(name)
        }
    }
}

pub(crate) struct ArgumentParser<T> {
    options: HashMap<&'static str, OptionHandler<T>>, // Long option lookup
    short_options: HashMap<&'static str, OptionHandler<T>>, // Short option lookup
    prefix_options: HashMap<&'static str, PrefixOptionHandler<T>>, // For options like -L, -l, etc.
    syntax: OptionSyntax,
}

impl<T: platform::Args> Default for ArgumentParser<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: platform::Args> ArgumentParser<T> {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::with_syntax(OptionSyntax::default())
    }

    #[must_use]
    pub(crate) fn with_syntax(syntax: OptionSyntax) -> Self {
        Self {
            options: HashMap::new(),
            short_options: HashMap::new(),
            prefix_options: HashMap::new(),
            syntax,
        }
    }

    pub(crate) fn declare(&mut self) -> OptionDeclaration<'_, T, NoParam> {
        OptionDeclaration {
            parser: self,
            long_names: Vec::new(),
            short_names: Vec::new(),
            prefixes: Vec::new(),
            sub_options: HashMap::new(),
            help_text: "",
            _phantom: std::marker::PhantomData,
        }
    }

    pub(crate) fn declare_with_param(&mut self) -> OptionDeclaration<'_, T, WithParam> {
        OptionDeclaration {
            parser: self,
            long_names: Vec::new(),
            short_names: Vec::new(),
            prefixes: Vec::new(),
            sub_options: HashMap::new(),
            help_text: "",
            _phantom: std::marker::PhantomData,
        }
    }

    pub(crate) fn declare_with_three_params(
        &mut self,
    ) -> OptionDeclaration<'_, T, WithThreeParams> {
        OptionDeclaration {
            parser: self,
            long_names: Vec::new(),
            short_names: Vec::new(),
            prefixes: Vec::new(),
            sub_options: HashMap::new(),
            help_text: "",
            _phantom: std::marker::PhantomData,
        }
    }

    pub(crate) fn declare_with_optional_param(
        &mut self,
    ) -> OptionDeclaration<'_, T, WithOptionalParam> {
        OptionDeclaration {
            parser: self,
            long_names: Vec::new(),
            short_names: Vec::new(),
            prefixes: Vec::new(),
            sub_options: HashMap::new(),
            help_text: "",
            _phantom: std::marker::PhantomData,
        }
    }

    pub(crate) fn handle_argument<S: AsRef<str>, I: Iterator<Item = S>>(
        &self,
        args: &mut T,
        modifier_stack: &mut Vec<Modifiers>,
        arg: &str,
        input: &mut I,
    ) -> Result<()> {
        // TODO @lapla-cogito standardize the interface. @file doesn't use a leading hyphen.
        // Handle `@file`option (recursively) - merging in the options contained in the file
        if let Some(path) = arg.strip_prefix('@') {
            let file_args = read_args_from_file(Path::new(path))?;
            let mut file_arg_iter = file_args.iter();
            while let Some(file_arg) = file_arg_iter.next() {
                self.handle_argument(args, modifier_stack, file_arg, &mut file_arg_iter)?;
            }
            return Ok(());
        }

        if let Some(stripped) = self.syntax.strip_prefix(arg) {
            // Check for an option that carries its value attached, e.g. `--foo=bar` or `/FOO:bar`.
            if let Some(sep_pos) = stripped.find(self.syntax.value_separator) {
                let option_name = &stripped[..sep_pos];
                let value = &stripped[sep_pos + self.syntax.value_separator.len_utf8()..];

                // The option as the user wrote it, without the value, for error messages.
                let arg_name = &arg[..arg.len() - stripped.len() + sep_pos];

                if let Some(handler) = self
                    .options
                    .get(self.syntax.lookup_key(option_name).as_ref())
                {
                    match &handler.handler {
                        OptionHandlerFn::WithParam(f) => {
                            ensure!(
                                !value.is_empty() || self.syntax.allows_separate_value,
                                "missing value for {arg_name}"
                            );
                            f(args, modifier_stack, value)?;
                        }
                        OptionHandlerFn::WithThreeParams(_) => {
                            bail!(
                                "multi-argument option cannot use the '{}' syntax",
                                self.syntax.value_separator
                            )
                        }
                        OptionHandlerFn::OptionalParam(f) => f(args, modifier_stack, Some(value))?,
                        OptionHandlerFn::NoParam(_) => {
                            ensure!(
                                self.syntax.allows_separate_value,
                                "{arg} does not take a value"
                            );
                            return Ok(());
                        }
                    }
                    return Ok(());
                }
            } else {
                let key = self.syntax.lookup_key(stripped);

                if let Some(handler) = self.options.get(key.as_ref()) {
                    // `--build-id` on its own means `--build-id=fast` rather than taking the
                    // following token as its value.
                    if key.as_ref() == "build-id"
                        && let OptionHandlerFn::WithParam(f) = &handler.handler
                    {
                        f(args, modifier_stack, "fast")?;
                        return Ok(());
                    }

                    self.invoke_handler(args, modifier_stack, arg, &handler.handler, input)?;
                    return Ok(());
                }
            }
        }

        if arg.starts_with('-') && !arg.starts_with("--") && arg.len() > 1 {
            let option_name = self.syntax.lookup_key(&arg[1..]);
            if let Some(handler) = self.short_options.get(option_name.as_ref()) {
                self.invoke_handler(args, modifier_stack, arg, &handler.handler, input)?;
                return Ok(());
            }
        }

        // Prefix options. These should be handled after processing long and short options,
        // because some options (like `-hashstyle=gnu`) can be misinterpreted as prefix options.
        for (prefix, handler) in &self.prefix_options {
            if let Some(rest) = arg.strip_prefix(&format!("-{prefix}")) {
                let value = if rest.is_empty() {
                    let next_arg = input
                        .next()
                        .with_context(|| format!("Missing argument to -{prefix}"))?;
                    next_arg.as_ref().to_owned()
                } else {
                    rest.to_owned()
                };

                if let Some((key, param_value)) = value.split_once('=') {
                    // Value has '=', look up key with trailing '='
                    if let Some(sub) = handler.sub_options.get(format!("{key}=").as_str()) {
                        match sub.handler {
                            SubOptionHandler::NoValue(_) => {
                                (handler.handler)(args, modifier_stack, &value)?;
                            }
                            SubOptionHandler::WithValue(f) => f(args, modifier_stack, param_value)?,
                        }
                    } else {
                        // Fall back to the main handler
                        (handler.handler)(args, modifier_stack, &value)?;
                    }
                } else {
                    // No '=' in value, look up exact match
                    if let Some(sub) = handler.sub_options.get(value.as_str()) {
                        match sub.handler {
                            SubOptionHandler::NoValue(f) => f(args, modifier_stack)?,
                            SubOptionHandler::WithValue(_) => {
                                bail!("Option -{prefix} {value} requires a value");
                            }
                        }
                    } else {
                        // Fall back to the main handler
                        (handler.handler)(args, modifier_stack, &value)?;
                    }
                }
                return Ok(());
            }
        }

        if self.syntax.starts_with_prefix(arg) {
            if let Some(stripped) = self.syntax.strip_prefix(arg)
                && args.is_ignored_flag(stripped)
            {
                args.warn_unsupported(arg)?;
                return Ok(());
            }

            args.common_mut().unrecognized_options.push(arg.to_owned());
            return Ok(());
        }

        let common = args.common_mut();
        common.save_dir.handle_file(arg);
        common.inputs.push(Input {
            spec: InputSpec::File(Box::from(Path::new(arg))),
            search_first: None,
            modifiers: *modifier_stack.last().unwrap(),
        });

        Ok(())
    }

    /// Runs `handler`, taking any values it needs from `input`. `arg` is the argument as it was
    /// written, for use in error messages.
    fn invoke_handler<S: AsRef<str>, I: Iterator<Item = S>>(
        &self,
        args: &mut T,
        modifier_stack: &mut Vec<Modifiers>,
        arg: &str,
        handler: &OptionHandlerFn<T>,
        input: &mut I,
    ) -> Result<()> {
        if !self.syntax.allows_separate_value
            && matches!(
                handler,
                OptionHandlerFn::WithParam(_) | OptionHandlerFn::WithThreeParams(_)
            )
        {
            bail!("missing value for {arg}");
        }

        match handler {
            OptionHandlerFn::NoParam(f) => f(args, modifier_stack)?,
            OptionHandlerFn::WithParam(f) => {
                let next_arg = input
                    .next()
                    .with_context(|| format!("Missing argument to {arg}"))?;
                f(args, modifier_stack, next_arg.as_ref())?;
            }
            OptionHandlerFn::WithThreeParams(f) => {
                let first_arg = input
                    .next()
                    .with_context(|| format!("Missing first argument to {arg}"))?;
                let second_arg = input
                    .next()
                    .with_context(|| format!("Missing second argument to {arg}"))?;
                let third_arg = input
                    .next()
                    .with_context(|| format!("Missing third argument to {arg}"))?;
                f(
                    args,
                    modifier_stack,
                    first_arg.as_ref(),
                    second_arg.as_ref(),
                    third_arg.as_ref(),
                )?;
            }
            OptionHandlerFn::OptionalParam(f) => {
                f(args, modifier_stack, None)?;
            }
        }

        Ok(())
    }

    #[must_use]
    pub(crate) fn generate_help(&self) -> String {
        const HELP_COL1_WIDTH: usize = 30;
        let mut help = String::new();
        help.push_str("USAGE:\n    wild [OPTIONS] [FILES...]\n\nOPTIONS:\n");

        let mut prefix_options = self.prefix_options.iter().collect_vec();
        prefix_options.sort_by_key(|(prefix, _)| *prefix);

        // TODO: This is ad-hoc
        help.push_str(&format!(
            "    {:<width$} Read options from a file\n",
            "@<VALUE>",
            width = HELP_COL1_WIDTH
        ));

        let mut help_to_options: HashMap<&str, Vec<String>> = HashMap::new();
        let mut processed_short_options: HashSet<&str> = HashSet::new();

        // Collect all long options and their associated short options
        for (long_name, handler) in &self.options {
            if !handler.help_text.is_empty() {
                let long_suffix = handler.handler.help_suffix_long();
                let mut option_names = vec![format!("--{long_name}{long_suffix}")];

                // Add associated short options
                let short_suffix = handler.handler.help_suffix_short();
                for short_char in &handler.short_names {
                    option_names.push(format!("-{short_char}{short_suffix}"));
                }

                help_to_options
                    .entry(handler.help_text)
                    .or_default()
                    .extend(option_names);
            }

            // Mark short options of help-less handlers as processed
            for short_name in &handler.short_names {
                processed_short_options.insert(short_name);
            }
        }

        for (prefix, handler) in prefix_options {
            if !processed_short_options.contains(prefix) && !handler.help_text.is_empty() {
                let option_name = format!("-{prefix} <VALUE>");
                help.push_str(&format!(
                    "    {option_name:<width$} {}\n",
                    handler.help_text,
                    width = HELP_COL1_WIDTH
                ));

                // Add sub-options if they exist
                let mut sub_options = handler.sub_options.iter().collect_vec();
                sub_options.sort_by_key(|(name, _)| *name);

                for (sub_name, sub) in sub_options {
                    let display_name = if sub.with_value() && sub_name.ends_with('=') {
                        // sub_name ends with '=' (e.g., "max-page-size="), so add <VALUE>
                        format!("{sub_name}<VALUE>")
                    } else {
                        sub_name.to_string()
                    };
                    let option_name = format!("-{prefix} {display_name}");
                    help.push_str(&format!(
                        "     {option_name:<width$} {sub_help}\n",
                        sub_help = sub.help,
                        width = HELP_COL1_WIDTH - 1
                    ));
                }
            }
        }

        // Add short-only options
        for (short_char, handler) in &self.short_options {
            if !processed_short_options.contains(short_char) && !handler.help_text.is_empty() {
                let short_suffix = handler.handler.help_suffix_short();
                help_to_options
                    .entry(handler.help_text)
                    .or_default()
                    .push(format!("-{short_char}{short_suffix}"));
            }
        }

        let mut sorted_help_groups = help_to_options.into_iter().collect_vec();
        sorted_help_groups.sort_by_key(|(_, option_names)| {
            option_names.iter().min().unwrap_or(&String::new()).clone()
        });

        for (help_text, mut option_names) in sorted_help_groups {
            option_names.sort_by(|a, b| {
                let a_is_short = a.len() == 2 && a.starts_with('-');
                let b_is_short = b.len() == 2 && b.starts_with('-');
                match (a_is_short, b_is_short) {
                    (true, false) => std::cmp::Ordering::Less, // short options first
                    (false, true) => std::cmp::Ordering::Greater, // long options after
                    _ => a.cmp(b),                             // same type, alphabetical
                }
            });

            let option_names_str = option_names.join(", ");
            help.push_str(&format!("    {option_names_str:<30} {help_text}\n"));
        }

        help
    }
}

impl<T> ArgumentParser<T> {
    fn insert_long_option(&mut self, name: &'static str, handler: OptionHandler<T>) {
        self.assert_valid_key(name);
        assert!(
            self.options.insert(name, handler).is_none(),
            "Option --{name} registered more than once"
        );
    }

    fn insert_short_option(&mut self, name: &'static str, handler: OptionHandler<T>) {
        self.assert_valid_key(name);
        self.short_options.insert(name, handler);
    }

    /// Options on case-insensitive platforms must be declared in lowercase, since arguments are
    /// lowercased before they're looked up.
    fn assert_valid_key(&self, name: &'static str) {
        if self.syntax.case_insensitive {
            assert!(
                !name.contains(|c: char| c.is_ascii_uppercase()),
                "Option {name} must be declared in lowercase"
            );
        }
    }
}

struct OptionHandler<T> {
    help_text: &'static str,
    handler: OptionHandlerFn<T>,
    short_names: Vec<&'static str>,
}

impl<T> Clone for OptionHandler<T> {
    fn clone(&self) -> Self {
        Self {
            help_text: self.help_text,
            handler: self.handler,
            short_names: self.short_names.clone(),
        }
    }
}

struct PrefixOptionHandler<T> {
    help_text: &'static str,
    handler: fn(&mut T, &mut Vec<Modifiers>, &str) -> Result<()>,
    sub_options: HashMap<&'static str, SubOption<T>>,
}

pub(crate) type OptionalParamHandler<T> =
    fn(&mut T, &mut Vec<Modifiers>, Option<&str>) -> Result<()>;
pub(crate) type ThreeParamHandler<T> =
    fn(&mut T, &mut Vec<Modifiers>, &str, &str, &str) -> Result<()>;

enum OptionHandlerFn<T> {
    NoParam(fn(&mut T, &mut Vec<Modifiers>) -> Result<()>),
    WithParam(fn(&mut T, &mut Vec<Modifiers>, &str) -> Result<()>),
    WithThreeParams(ThreeParamHandler<T>),
    OptionalParam(OptionalParamHandler<T>),
}

impl<T> Clone for OptionHandlerFn<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for OptionHandlerFn<T> {}

impl<T> OptionHandlerFn<T> {
    fn help_suffix_long(&self) -> &'static str {
        match self {
            OptionHandlerFn::NoParam(_) => "",
            OptionHandlerFn::WithParam(_) => "=<VALUE>",
            OptionHandlerFn::WithThreeParams(_) => "=<VALUE> <VALUE> <VALUE>",
            OptionHandlerFn::OptionalParam(_) => "[=<VALUE>]",
        }
    }

    fn help_suffix_short(&self) -> &'static str {
        match self {
            OptionHandlerFn::NoParam(_) => "",
            OptionHandlerFn::WithParam(_) => " <VALUE>",
            OptionHandlerFn::WithThreeParams(_) => " <VALUE> <VALUE> <VALUE>",
            OptionHandlerFn::OptionalParam(_) => " [<VALUE>]",
        }
    }
}

pub(crate) struct OptionDeclaration<'a, T, S> {
    parser: &'a mut ArgumentParser<T>,
    long_names: Vec<&'static str>,
    short_names: Vec<&'static str>,
    prefixes: Vec<&'static str>,
    sub_options: HashMap<&'static str, SubOption<T>>,
    help_text: &'static str,
    _phantom: std::marker::PhantomData<S>,
}

pub(crate) struct NoParam;
pub(crate) struct WithParam;
pub(crate) struct WithThreeParams;
pub(crate) struct WithOptionalParam;

enum SubOptionHandler<T> {
    /// Handler without value parameter (exact match)
    NoValue(fn(&mut T, &mut Vec<Modifiers>) -> Result<()>),
    /// Handler with value parameter (prefix match)
    WithValue(fn(&mut T, &mut Vec<Modifiers>, &str) -> Result<()>),
}

impl<T> Clone for SubOptionHandler<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for SubOptionHandler<T> {}

struct SubOption<T> {
    help: &'static str,
    handler: SubOptionHandler<T>,
}

impl<T> Clone for SubOption<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for SubOption<T> {}

impl<T> SubOption<T> {
    fn with_value(&self) -> bool {
        matches!(self.handler, SubOptionHandler::WithValue(_))
    }
}

impl<'a, T, S> OptionDeclaration<'a, T, S> {
    #[must_use]
    pub(crate) fn long(mut self, name: &'static str) -> Self {
        self.long_names.push(name);
        self
    }

    #[must_use]
    pub(crate) fn short(mut self, option: &'static str) -> Self {
        self.short_names.push(option);
        self
    }

    #[must_use]
    pub(crate) fn help(mut self, text: &'static str) -> Self {
        self.help_text = text;
        self
    }

    pub(crate) fn prefix(mut self, prefix: &'static str) -> Self {
        self.prefixes.push(prefix);
        self
    }

    #[must_use]
    pub(crate) fn sub_option(
        mut self,
        name: &'static str,
        help: &'static str,
        handler: fn(&mut T, &mut Vec<Modifiers>) -> Result<()>,
    ) -> Self {
        self.sub_options.insert(
            name,
            SubOption {
                help,
                handler: SubOptionHandler::NoValue(handler),
            },
        );
        self
    }

    #[must_use]
    pub(crate) fn sub_option_with_value(
        mut self,
        name: &'static str,
        help: &'static str,
        handler: fn(&mut T, &mut Vec<Modifiers>, &str) -> Result<()>,
    ) -> Self {
        self.sub_options.insert(
            name,
            SubOption {
                help,
                handler: SubOptionHandler::WithValue(handler),
            },
        );
        self
    }
}

impl<'a, T> OptionDeclaration<'a, T, NoParam> {
    pub(crate) fn execute(self, handler: fn(&mut T, &mut Vec<Modifiers>) -> Result<()>) {
        let option_handler = OptionHandler {
            help_text: self.help_text,
            handler: OptionHandlerFn::NoParam(handler),
            short_names: self.short_names.clone(),
        };

        for name in self.long_names {
            self.parser.insert_long_option(name, option_handler.clone());
        }

        for option in self.short_names {
            self.parser
                .insert_short_option(option, option_handler.clone());
        }
    }
}

impl<'a, T> OptionDeclaration<'a, T, WithParam> {
    pub(crate) fn execute(self, handler: fn(&mut T, &mut Vec<Modifiers>, &str) -> Result<()>) {
        let mut short_names = self.short_names.clone();
        short_names.extend_from_slice(&self.prefixes);

        let option_handler = OptionHandler {
            help_text: self.help_text,
            handler: OptionHandlerFn::WithParam(handler),
            short_names,
        };

        for name in self.long_names {
            self.parser.insert_long_option(name, option_handler.clone());
        }

        for option in self.short_names {
            self.parser
                .insert_short_option(option, option_handler.clone());
        }

        for prefix in self.prefixes {
            let prefix_handler = PrefixOptionHandler {
                help_text: self.help_text,
                sub_options: self.sub_options.clone(),
                handler,
            };

            self.parser.prefix_options.insert(prefix, prefix_handler);
        }
    }
}

impl<'a, T> OptionDeclaration<'a, T, WithThreeParams> {
    pub(crate) fn execute(self, handler: ThreeParamHandler<T>) {
        let option_handler = OptionHandler {
            help_text: self.help_text,
            handler: OptionHandlerFn::WithThreeParams(handler),
            short_names: self.short_names.clone(),
        };

        for name in self.long_names {
            self.parser.insert_long_option(name, option_handler.clone());
        }

        for option in self.short_names {
            self.parser
                .insert_short_option(option, option_handler.clone());
        }
    }
}

impl<'a, T> OptionDeclaration<'a, T, WithOptionalParam> {
    pub(crate) fn execute(self, handler: OptionalParamHandler<T>) {
        let option_handler = OptionHandler {
            help_text: self.help_text,
            handler: OptionHandlerFn::OptionalParam(handler),
            short_names: self.short_names.clone(),
        };

        for name in self.long_names {
            self.parser.insert_long_option(name, option_handler.clone());
        }

        for option in self.short_names {
            self.parser
                .insert_short_option(option, option_handler.clone());
        }
    }
}

pub(crate) fn parse_number(s: &str) -> Result<u64> {
    crate::parsing::parse_number(s).map_err(|()| crate::error!("Invalid number: {s}"))
}

pub(crate) fn read_args_from_file(path: &Path) -> Result<Vec<String>> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read arguments from file `{}`", path.display()))?;
    arguments_from_string(&contents)
}

/// Parses arguments from a string, handling quoting, escapes etc.
/// All arguments must be surrounded by a white space.
pub(crate) fn arguments_from_string(input: &str) -> Result<Vec<String>> {
    const QUOTES: [char; 2] = ['\'', '"'];

    let mut out = Vec::new();
    let mut chars = input.chars();
    let mut heap = None;
    let mut quote = None;
    let mut expect_whitespace = false;

    loop {
        let Some(mut ch) = chars.next() else {
            if let Some(quote) = quote.take() {
                bail!("Missing closing '{quote}'");
            }
            if let Some(arg) = heap.take() {
                out.push(arg);
            }
            break;
        };

        ensure!(
            !expect_whitespace || ch.is_whitespace(),
            "Expected white space after quoted argument"
        );
        expect_whitespace = false;

        if QUOTES.contains(&ch) {
            if let Some(qchr) = quote {
                if qchr == ch {
                    // close the argument
                    if let Some(arg) = heap.take() {
                        out.push(arg);
                    }
                    quote = None;
                    expect_whitespace = true;
                } else {
                    // accept the other quoting character as normal char
                    heap.get_or_insert(String::new()).push(ch);
                }
            } else {
                // beginning of a new argument
                ensure!(heap.is_none(), "Missing opening quote '{ch}'");
                quote = Some(ch);
            }
        } else if ch.is_whitespace() {
            if quote.is_none() {
                if let Some(arg) = heap.take() {
                    out.push(arg);
                }
            } else {
                heap.get_or_insert(String::new()).push(ch);
            }
        } else {
            if ch == '\\' {
                ch = chars.next().context("Invalid escape")?;
            }
            heap.get_or_insert(String::new()).push(ch);
        }
    }

    Ok(out)
}
