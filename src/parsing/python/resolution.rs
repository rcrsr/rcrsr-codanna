//! Python-specific resolution and inheritance implementation
//!
//! This module implements Python's unique scoping rules:
//! - LEGB rule: Local, Enclosing, Global, Built-in
//! - Class inheritance with Method Resolution Order (MRO)
//! - Module imports with aliasing

use crate::parsing::resolution::ImportBinding;
use crate::parsing::{InheritanceResolver, ResolutionScope, ScopeLevel, ScopeType};
use crate::{FileId, SymbolId};
use std::collections::HashMap;

/// Type alias for import information: (name, optional_alias)
type ImportInfo = (String, Option<String>);

/// Type alias for module imports: module_path -> list of imports
type ModuleImports = Vec<(String, Vec<ImportInfo>)>;

/// Python-specific resolution context implementing LEGB scoping rules
///
/// Python has a specific resolution order (LEGB):
/// 1. Local scope (function/method variables)
/// 2. Enclosing scope (nested functions)
/// 3. Global scope (module level)
/// 4. Built-in scope (Python built-ins)
pub struct PythonResolutionContext {
    #[allow(dead_code)]
    file_id: FileId,

    /// Local variables in current function/method
    local_scope: HashMap<String, SymbolId>,

    /// Variables from enclosing functions (closures)
    enclosing_scope: HashMap<String, SymbolId>,

    /// Module-level symbols (functions, classes, globals)
    global_scope: HashMap<String, SymbolId>,

    /// Imported symbols (from imports)
    imported_symbols: HashMap<String, SymbolId>,

    /// Built-in symbols (would need external data)
    builtin_scope: HashMap<String, SymbolId>,

    /// Track nested scopes
    scope_stack: Vec<ScopeType>,

    /// Import tracking (module_path -> list of (name, alias) pairs)
    imports: ModuleImports,

    /// Track current class for method resolution
    current_class: Option<String>,

    /// Binding info for imports keyed by visible name
    import_bindings: HashMap<String, ImportBinding>,
}

impl PythonResolutionContext {
    pub fn new(file_id: FileId) -> Self {
        Self {
            file_id,
            local_scope: HashMap::new(),
            enclosing_scope: HashMap::new(),
            global_scope: HashMap::new(),
            imported_symbols: HashMap::new(),
            builtin_scope: HashMap::new(),
            scope_stack: Vec::new(),
            imports: Vec::new(),
            current_class: None,
            import_bindings: HashMap::new(),
        }
    }

    /// Add an import (from module import name as alias)
    pub fn add_import(&mut self, module: String, name: String, alias: Option<String>) {
        // Find or create the module entry
        if let Some(entry) = self.imports.iter_mut().find(|(m, _)| m == &module) {
            entry.1.push((name, alias));
        } else {
            self.imports.push((module, vec![(name, alias)]));
        }
    }

    /// Add a symbol to the appropriate scope based on Python semantics
    pub fn add_symbol_python(&mut self, name: String, symbol_id: SymbolId, is_global: bool) {
        if is_global || self.scope_stack.is_empty() || self.scope_stack.len() == 1 {
            // Module level or explicitly global
            self.global_scope.insert(name, symbol_id);
        } else {
            // Local to current function
            self.local_scope.insert(name, symbol_id);
        }
    }

    /// Move local scope to enclosing when entering nested function
    pub fn push_enclosing_scope(&mut self) {
        // Move current locals to enclosing
        let locals = std::mem::take(&mut self.local_scope);
        for (name, id) in locals {
            self.enclosing_scope.insert(name, id);
        }
    }

    /// Clear enclosing scope when exiting nested function
    pub fn pop_enclosing_scope(&mut self) {
        self.enclosing_scope.clear();
    }
}

impl ResolutionScope for PythonResolutionContext {
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn add_symbol(&mut self, name: String, symbol_id: SymbolId, scope_level: ScopeLevel) {
        match scope_level {
            ScopeLevel::Local => {
                self.local_scope.insert(name, symbol_id);
            }
            ScopeLevel::Module => {
                self.global_scope.insert(name, symbol_id);
            }
            ScopeLevel::Package => {
                // In Python, package level is imported symbols
                self.imported_symbols.insert(name, symbol_id);
            }
            ScopeLevel::Global => {
                // In Python, this is truly global (module level)
                self.global_scope.insert(name, symbol_id);
            }
        }
    }

    fn resolve(&self, name: &str) -> Option<SymbolId> {
        // Python LEGB resolution order

        // 1. Local scope
        if let Some(&id) = self.local_scope.get(name) {
            return Some(id);
        }

        // 2. Enclosing scope (for nested functions)
        if let Some(&id) = self.enclosing_scope.get(name) {
            return Some(id);
        }

        // 3. Global (module) scope
        if let Some(&id) = self.global_scope.get(name) {
            return Some(id);
        }

        // 4. Imported symbols
        if let Some(&id) = self.imported_symbols.get(name) {
            return Some(id);
        }

        // 5. Built-in scope (would need external data)
        if let Some(&id) = self.builtin_scope.get(name) {
            return Some(id);
        }

        // 6. Check if it's a qualified name (contains .)
        if name.contains('.') {
            // CRITICAL FIX: First try to resolve the full qualified path directly
            // This handles cases where we have the full module path stored (e.g., "myapp.utils.helper.process")
            // Check in all scopes for the full qualified name
            if let Some(&id) = self.imported_symbols.get(name) {
                return Some(id);
            }
            if let Some(&id) = self.global_scope.get(name) {
                return Some(id);
            }

            // If full path not found, try to resolve as a 2-part path
            let parts: Vec<&str> = name.split('.').collect();
            if parts.len() == 2 {
                let module_or_class = parts[0];
                let function_or_method = parts[1];

                // Check if module/class exists in our codebase
                if self.resolve(module_or_class).is_some() {
                    // Module/class exists, resolve the function/method
                    return self.resolve(function_or_method);
                }
                // External library (like os.path) - return None
                return None;
            }
        }

        None
    }

    fn clear_local_scope(&mut self) {
        self.local_scope.clear();
    }

    fn enter_scope(&mut self, scope_type: ScopeType) {
        // When entering a nested function, move locals to enclosing
        if matches!(scope_type, ScopeType::Function { .. }) && !self.scope_stack.is_empty() {
            self.push_enclosing_scope();
        }
        self.scope_stack.push(scope_type);
    }

    fn exit_scope(&mut self) {
        if let Some(scope) = self.scope_stack.pop() {
            match scope {
                ScopeType::Function { .. } => {
                    self.clear_local_scope();
                    self.pop_enclosing_scope();
                }
                ScopeType::Class => {
                    self.current_class = None;
                }
                _ => {}
            }
        }
    }

    fn symbols_in_scope(&self) -> Vec<(String, SymbolId, ScopeLevel)> {
        let mut symbols = Vec::new();

        // Add all symbols with their appropriate scope levels
        for (name, &id) in &self.local_scope {
            symbols.push((name.clone(), id, ScopeLevel::Local));
        }
        for (name, &id) in &self.imported_symbols {
            symbols.push((name.clone(), id, ScopeLevel::Package));
        }
        for (name, &id) in &self.global_scope {
            symbols.push((name.clone(), id, ScopeLevel::Global));
        }

        symbols
    }

    fn resolve_relationship(
        &self,
        _from_name: &str,
        to_name: &str,
        kind: crate::RelationKind,
        _from_file: FileId,
    ) -> Option<SymbolId> {
        use crate::RelationKind;

        match kind {
            RelationKind::Defines => {
                // Python: methods are always on classes, no trait/inherent distinction
                // Decorators like @property, @classmethod, @staticmethod don't change resolution
                // Just resolve the method name directly
                self.resolve(to_name)
            }
            RelationKind::Calls => {
                // Python: handle module.function patterns
                if to_name.contains('.') {
                    // Module qualified name like json.loads or os.path.join
                    // Try to resolve the full qualified name first
                    if let Some(id) = self.resolve(to_name) {
                        return Some(id);
                    }
                    // If not found, try just the function name
                    // (might be imported directly)
                    if let Some(last_part) = to_name.rsplit('.').next() {
                        return self.resolve(last_part);
                    }
                }
                // Simple function call
                self.resolve(to_name)
            }
            RelationKind::Extends => {
                // Python: inheritance from base classes
                // Just resolve the base class name
                self.resolve(to_name)
            }
            _ => {
                // For other relationship types, use standard resolution
                self.resolve(to_name)
            }
        }
    }

    fn populate_imports(&mut self, imports: &[crate::parsing::Import]) {
        // Convert Import records into our internal format: (module_path, vec[(name, alias)])
        for import in imports {
            // Extract module and name from the import path
            // For "from myapp.utils import helper", we store module="myapp.utils", name="helper"
            if let Some(last_dot) = import.path.rfind('.') {
                let module = import.path[..last_dot].to_string();
                let name = import.path[last_dot + 1..].to_string();
                self.add_import(module, name, import.alias.clone());
            } else {
                // Simple import like "import os" - module is the path, name is empty
                self.add_import(import.path.clone(), String::new(), import.alias.clone());
            }
        }
    }

    fn register_import_binding(&mut self, binding: ImportBinding) {
        self.import_bindings
            .insert(binding.exposed_name.clone(), binding);
    }

    fn import_binding(&self, name: &str) -> Option<ImportBinding> {
        self.import_bindings.get(name).cloned()
    }
}

/// Python class inheritance resolver
///
/// Handles Python's Method Resolution Order (MRO) and multiple inheritance
#[derive(Clone)]
pub struct PythonInheritanceResolver {
    /// Maps class names to their base classes
    /// Key: "ClassName", Value: Vec<"BaseClass">
    class_bases: HashMap<String, Vec<String>>,

    /// Maps class names to their methods
    /// Key: "ClassName", Value: Vec<"method_name">
    class_methods: HashMap<String, Vec<String>>,

    /// Cached MRO for classes (Method Resolution Order)
    /// Key: "ClassName", Value: Vec<"ClassName"> (in MRO order)
    mro_cache: HashMap<String, Vec<String>>,
}

impl Default for PythonInheritanceResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl PythonInheritanceResolver {
    pub fn new() -> Self {
        Self {
            class_bases: HashMap::new(),
            class_methods: HashMap::new(),
            mro_cache: HashMap::new(),
        }
    }

    /// Calculate Method Resolution Order (MRO) using C3 linearization
    /// This is a simplified version - Python's actual MRO is more complex
    fn calculate_mro(&self, class_name: &str) -> Vec<String> {
        // Check cache first
        if let Some(mro) = self.mro_cache.get(class_name) {
            return mro.clone();
        }

        // Simple MRO: class itself, then bases in order (left-to-right)
        let mut mro = vec![class_name.to_string()];

        if let Some(bases) = self.class_bases.get(class_name) {
            for base in bases {
                // Recursively get MRO of base classes
                let base_mro = self.calculate_mro(base);
                for class in base_mro {
                    if !mro.contains(&class) {
                        mro.push(class);
                    }
                }
            }
        }

        mro
    }

    /// Add a class with its base classes
    pub fn add_class(&mut self, class_name: String, bases: Vec<String>) {
        self.class_bases.insert(class_name.clone(), bases);
        // Clear MRO cache as hierarchy changed
        self.mro_cache.clear();
    }

    /// Add methods to a class
    pub fn add_class_methods(&mut self, class_name: String, methods: Vec<String>) {
        self.class_methods.insert(class_name, methods);
    }
}

impl InheritanceResolver for PythonInheritanceResolver {
    fn add_inheritance(&mut self, child: String, parent: String, kind: &str) {
        if kind == "extends" || kind == "inherits" {
            // In Python, this is class inheritance
            self.class_bases.entry(child).or_default().push(parent);
            // Clear MRO cache as hierarchy changed
            self.mro_cache.clear();
        }
    }

    fn resolve_method(&self, type_name: &str, method_name: &str) -> Option<String> {
        // Get MRO for the class
        let mro = self.calculate_mro(type_name);

        // Search for method in MRO order
        for class in &mro {
            if let Some(methods) = self.class_methods.get(class)
                && methods.iter().any(|m| m == method_name)
            {
                return Some(class.clone());
            }
        }

        None
    }

    fn get_inheritance_chain(&self, type_name: &str) -> Vec<String> {
        self.calculate_mro(type_name)
    }

    fn is_subtype(&self, child: &str, parent: &str) -> bool {
        let mro = self.calculate_mro(child);
        mro.contains(&parent.to_string())
    }

    fn add_type_methods(&mut self, type_name: String, methods: Vec<String>) {
        self.add_class_methods(type_name, methods);
    }

    fn get_all_methods(&self, type_name: &str) -> Vec<String> {
        let mut all_methods = Vec::new();
        let mro = self.calculate_mro(type_name);

        for class in &mro {
            if let Some(methods) = self.class_methods.get(class) {
                for method in methods {
                    if !all_methods.contains(method) {
                        all_methods.push(method.clone());
                    }
                }
            }
        }

        all_methods
    }
}
