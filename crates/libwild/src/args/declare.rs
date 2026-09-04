use super::parse::ArgumentParser;
use super::types::*;
use crate::fs::FileReplacementMode;
use crate::fs::FileWriteMode;
use crate::platform;
use std::num::NonZeroUsize;

pub(crate) fn declare_common_args<T: platform::Args>(parser: &mut ArgumentParser<T>) {
    parser
        .declare()
        .long("write-layout")
        .execute(|args, _modifier_stack| {
            args.common_mut().write_layout = true;
            Ok(())
        });

    parser
        .declare()
        .long("write-trace")
        .execute(|args, _modifier_stack| {
            args.common_mut().write_trace = true;
            Ok(())
        });

    parser
        .declare_with_param()
        .long("sym-info")
        .help("Show symbol information. Accepts symbol name or ID.")
        .execute(|args, _modifier_stack, value| {
            args.common_mut().sym_info = Some(value.to_owned());
            Ok(())
        });

    parser
        .declare()
        .long("validate-output")
        .execute(|args, _modifier_stack| {
            args.common_mut().validate_output = true;
            Ok(())
        });

    parser
        .declare()
        .long("update-in-place")
        .help("Update file in place")
        .execute(|args, _modifier_stack| {
            args.common_mut().file_replacement_mode = Some(FileReplacementMode::UpdateInPlace);
            Ok(())
        });

    parser
        .declare_with_optional_param()
        .long("time")
        .help("Show timing information")
        .execute(|args, _modifier_stack, value| {
            args.common_mut().time_phase_options = match value {
                Some(v) => Some(parse_time_phase_options(v)?),
                None => Some(Vec::new()),
            };
            Ok(())
        });

    parser
        .declare()
        .long("mmap-output-file")
        .help("Write output file using mmap (default)")
        .execute(|args, _modifier_stack| {
            args.common_mut().file_write_mode = Some(FileWriteMode::Mmap);
            Ok(())
        });

    parser
        .declare()
        .long("no-mmap-output-file")
        .help("Write output file without mmap")
        .execute(|args, _modifier_stack| {
            args.common_mut().file_write_mode = Some(FileWriteMode::BufferThenWrite);
            Ok(())
        });

    parser
        .declare()
        .long("fallocate-output-file")
        .help("Preallocate space for the output file with fallocate")
        .execute(|args, _modifier_stack| {
            args.common_mut().fallocate_output_file = Some(true);
            Ok(())
        });

    parser
        .declare()
        .long("no-fallocate-output-file")
        .help("Do not preallocate space for the output file with fallocate")
        .execute(|args, _modifier_stack| {
            args.common_mut().fallocate_output_file = Some(false);
            Ok(())
        });

    parser
        .declare()
        .long("madvise-huge-pages")
        .help("Request transparent huge pages for the output file mmap")
        .execute(|args, _modifier_stack| {
            args.common_mut().madvise_huge_pages = Some(true);
            Ok(())
        });

    parser
        .declare()
        .long("no-madvise-huge-pages")
        .help("Do not request transparent huge pages for the output file mmap")
        .execute(|args, _modifier_stack| {
            args.common_mut().madvise_huge_pages = Some(false);
            Ok(())
        });

    parser
        .declare_with_optional_param()
        .long("threads")
        .help("Use multiple threads for linking")
        .execute(|args, _modifier_stack, value| {
            match value {
                Some(v) => {
                    args.common_mut().num_threads =
                        Some(NonZeroUsize::try_from(v.parse::<usize>()?)?);
                }
                None => {
                    args.common_mut().num_threads = None; // Default behaviour
                }
            }
            Ok(())
        });

    parser
        .declare()
        .long("no-threads")
        .help("Use a single thread")
        .execute(|args, _modifier_stack| {
            args.common_mut().num_threads = Some(NonZeroUsize::new(1).unwrap());
            Ok(())
        });

    parser
        .declare()
        .long("no-fork")
        .help("Do not fork while linking")
        .execute(|args, _modifier_stack| {
            args.common_mut().should_fork = false;
            Ok(())
        });

    parser
        .declare()
        .long("fork")
        .help("Spawn a child process to link (default)")
        .execute(|args, _modifier_stack| {
            args.common_mut().should_fork = true;
            Ok(())
        });

    parser
        .declare()
        .long("incremental")
        .help("Enable incremental linking (see also WILD_INCREMENTAL=1)")
        .execute(|args, _modifier_stack| {
            args.common_mut().incremental = true;
            Ok(())
        });
}
