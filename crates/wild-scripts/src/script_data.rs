#[derive(Clone, Copy)]
pub struct ScriptData<'data> {
    pub raw: &'data [u8],
}
