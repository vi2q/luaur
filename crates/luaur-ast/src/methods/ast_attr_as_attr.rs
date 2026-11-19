use crate::records::ast_attr::AstAttr;

impl AstAttr {
    pub fn as_attr(&mut self) -> *mut AstAttr {
        self as *mut AstAttr
    }
}

pub extern "C" fn ast_attr_as_attr(this: *mut AstAttr) -> *mut AstAttr {
    unsafe { (*this).as_attr() }
}
