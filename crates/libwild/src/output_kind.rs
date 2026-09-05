use crate::FileSystem;
use crate::input_data::FileLoader;
use crate::platform;
#[allow(unused_imports)]
pub(crate) use crate::platform::OutputKind;
use crate::platform::RelocationModel;

impl OutputKind {
    pub(crate) fn new(
        args: &impl platform::Args,
        input_data: &FileLoader<'_, impl FileSystem>,
    ) -> OutputKind {
        let model = args.relocation_model();
        if !args.should_output_executable() {
            if args.should_output_partial_object() {
                return OutputKind::PartialLink;
            }
            OutputKind::SharedObject
        } else if args.dynamic_linker().is_some() && model == RelocationModel::PositionIndependent {
            // GNU ld turns static position-independent executables into dynamic ones if a dynamic
            // linker is set.
            OutputKind::DynamicExecutable(model)
        } else if input_data.has_dynamic {
            // When attempting to create static executable, but DSO is added as an input we need to
            // proceed with dynamic executable.
            // This is in line with LLD, but GNU ld goes a step further: if no DSO ends up loaded,
            // it'll go back to static one. This would add a lot of complexity with the
            // current design, so we just stick to LLD behaviour.
            OutputKind::DynamicExecutable(model)
        } else {
            OutputKind::StaticExecutable(model)
        }
    }
}
