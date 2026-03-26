pub struct ToolCatalog {}

pub trait ToolBag {
    fn category(&self) -> ToolCategory;
}
