//! C-specific resolution implementation
//!
//! This module provides C language resolution following the same pattern
//! as Rust and TypeScript implementations.

use crate::parsing::resolution::ImportBinding;
use crate::parsing::{InheritanceResolver, ResolutionScope, ScopeLevel, ScopeType};
use crate::{FileId, SymbolId};
use std::collections::HashMap;

/// C-specific resolution context implementing C scoping rules
///
/// C has simpler scoping than Rust or TypeScript:
/// 1. Local scope (function parameters, local variables)
/// 2. File scope (functions, global variables, types)
/// 3. External linkage (symbols from included headers)
pub struct CResolutionContext {
    #[allow(dead_code)]
    file_id: FileId, // Will be used for file-specific resolution

    /// Local variables and parameters in current scope
    local_scope: HashMap<String, SymbolId>,

    /// File-level symbols (functions, global variables)
    module_symbols: HashMap<String, SymbolId>,

    /// Symbols from included headers
    imported_symbols: HashMap<String, SymbolId>,

    /// Global symbols visible across the project
    global_symbols: HashMap<String, SymbolId>,

    /// Track nested scopes (functions, blocks, etc.)
    scope_stack: Vec<ScopeType>,

    /// Include tracking (header paths)
    includes: Vec<String>,

    /// Binding info for imports keyed by visible name
    import_bindings: HashMap<String, ImportBinding>,
}

impl CResolutionContext {
    pub fn new(file_id: FileId) -> Self {
        Self {
            file_id,
            local_scope: HashMap::new(),
            module_symbols: HashMap::new(),
            imported_symbols: HashMap::new(),
            global_symbols: HashMap::new(),
            scope_stack: Vec::new(),
            includes: Vec::new(),
            import_bindings: HashMap::new(),
        }
    }

    /// Add an include directive
    pub fn add_include(&mut self, header_path: String) {
        self.includes.push(header_path);
    }

    /// Add a local variable or parameter to the current scope
    pub fn add_local(&mut self, name: String, symbol_id: SymbolId) {
        self.local_scope.insert(name, symbol_id);
    }

    /// Add a file-level symbol (function, global variable, type)
    pub fn add_module_symbol(&mut self, name: String, symbol_id: SymbolId) {
        self.module_symbols.insert(name, symbol_id);
    }

    /// Add an imported symbol from a header file
    pub fn add_import_symbol(&mut self, name: String, symbol_id: SymbolId) {
        self.imported_symbols.insert(name, symbol_id);
    }

    /// Add a global symbol visible project-wide
    pub fn add_global_symbol(&mut self, name: String, symbol_id: SymbolId) {
        self.global_symbols.insert(name, symbol_id);
    }

    /// Get include list for this file
    pub fn includes(&self) -> &[String] {
        &self.includes
    }
}

impl ResolutionScope for CResolutionContext {
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn add_symbol(&mut self, name: String, symbol_id: SymbolId, scope_level: ScopeLevel) {
        match scope_level {
            ScopeLevel::Local => {
                self.local_scope.insert(name, symbol_id);
            }
            ScopeLevel::Module => {
                self.module_symbols.insert(name, symbol_id);
            }
            ScopeLevel::Package => {
                // In C, Package level maps to imported symbols from headers
                self.imported_symbols.insert(name, symbol_id);
            }
            ScopeLevel::Global => {
                // Global symbols visible across the project
                self.global_symbols.insert(name, symbol_id);
            }
        }
    }

    fn resolve(&self, name: &str) -> Option<SymbolId> {
        // C resolution order: local → module → imported → global

        // 1. Check local scope first
        if let Some(&id) = self.local_scope.get(name) {
            return Some(id);
        }

        // 2. Check file-level symbols
        if let Some(&id) = self.module_symbols.get(name) {
            return Some(id);
        }

        // 3. Check imported symbols from headers
        if let Some(&id) = self.imported_symbols.get(name) {
            return Some(id);
        }

        // 4. Check global symbols
        if let Some(&id) = self.global_symbols.get(name) {
            return Some(id);
        }

        None
    }

    fn clear_local_scope(&mut self) {
        self.local_scope.clear();
    }

    fn enter_scope(&mut self, scope_type: ScopeType) {
        self.scope_stack.push(scope_type);
        // C doesn't have hoisting, so entering a scope doesn't affect resolution
    }

    fn exit_scope(&mut self) {
        self.scope_stack.pop();
        // Clear locals when exiting function scope
        if matches!(
            self.scope_stack.last(),
            None | Some(ScopeType::Module | ScopeType::Global)
        ) {
            self.clear_local_scope();
        }
    }

    fn symbols_in_scope(&self) -> Vec<(String, SymbolId, ScopeLevel)> {
        let mut symbols = Vec::new();

        // Add all symbols with their appropriate scope levels
        for (name, &id) in &self.local_scope {
            symbols.push((name.clone(), id, ScopeLevel::Local));
        }
        for (name, &id) in &self.module_symbols {
            symbols.push((name.clone(), id, ScopeLevel::Module));
        }
        for (name, &id) in &self.imported_symbols {
            symbols.push((name.clone(), id, ScopeLevel::Package));
        }
        for (name, &id) in &self.global_symbols {
            symbols.push((name.clone(), id, ScopeLevel::Global));
        }

        symbols
    }

    fn populate_imports(&mut self, imports: &[crate::parsing::Import]) {
        // Store raw import paths (header files in C)
        for import in imports {
            self.includes.push(import.path.clone());
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

/// Implementation of InheritanceResolver for C
///
/// C doesn't have traditional inheritance, but it does have:
/// - Struct composition (embedding one struct in another)
/// - Typedef relationships (type aliases)
/// - Function pointers that act as "methods" for structs
pub struct CInheritanceResolver {
    /// Maps type name -> composed/embedded types
    /// For C, this represents struct composition rather than inheritance
    composition_map: HashMap<String, Vec<(String, String)>>,

    /// Maps type name -> function pointers/methods associated with the type
    type_methods: HashMap<String, Vec<String>>,

    /// Maps typedef aliases to their underlying types
    typedef_map: HashMap<String, String>,
}

impl CInheritanceResolver {
    pub fn new() -> Self {
        Self {
            composition_map: HashMap::new(),
            type_methods: HashMap::new(),
            typedef_map: HashMap::new(),
        }
    }

    /// Add a typedef relationship
    pub fn add_typedef(&mut self, alias: String, underlying_type: String) {
        self.typedef_map.insert(alias, underlying_type);
    }

    /// Resolve typedef chain to get the final underlying type
    pub fn resolve_typedef(&self, type_name: &str) -> String {
        let mut current = type_name.to_string();
        let mut visited = std::collections::HashSet::new();

        while let Some(underlying) = self.typedef_map.get(&current) {
            if visited.contains(&current) {
                break; // Circular typedef, shouldn't happen in valid C
            }
            visited.insert(current.clone());
            current = underlying.clone();
        }

        current
    }
}

impl Default for CInheritanceResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl InheritanceResolver for CInheritanceResolver {
    fn add_inheritance(&mut self, child: String, parent: String, kind: &str) {
        // In C, "inheritance" is really composition or typedef relationships
        match kind {
            "typedef" => {
                self.typedef_map.insert(child, parent);
            }
            "composition" | "embedded" => {
                self.composition_map
                    .entry(child)
                    .or_default()
                    .push((parent, kind.to_string()));
            }
            _ => {
                // Default to composition for other relationships
                self.composition_map
                    .entry(child)
                    .or_default()
                    .push((parent, kind.to_string()));
            }
        }
    }

    fn resolve_method(&self, type_name: &str, method: &str) -> Option<String> {
        // Resolve through typedef chain first
        let resolved_type = self.resolve_typedef(type_name);

        // Check if the type itself defines the method (function pointer)
        if let Some(methods) = self.type_methods.get(&resolved_type)
            && methods.contains(&method.to_string())
        {
            return Some(resolved_type);
        }

        // Check composed/embedded types
        if let Some(composed_types) = self.composition_map.get(&resolved_type) {
            for (composed_type, _kind) in composed_types {
                if let Some(provider) = self.resolve_method(composed_type, method) {
                    return Some(provider);
                }
            }
        }

        None
    }

    fn get_inheritance_chain(&self, type_name: &str) -> Vec<String> {
        let mut chain = Vec::new();
        let mut visited = std::collections::HashSet::new();

        self.build_composition_chain(type_name, &mut chain, &mut visited);
        chain
    }

    fn is_subtype(&self, child: &str, parent: &str) -> bool {
        if child == parent {
            return true;
        }

        // Resolve through typedef chains
        let resolved_child = self.resolve_typedef(child);
        let resolved_parent = self.resolve_typedef(parent);

        if resolved_child == resolved_parent {
            return true;
        }

        let mut visited = std::collections::HashSet::new();
        self.is_composed_of_recursive(&resolved_child, &resolved_parent, &mut visited)
    }

    fn add_type_methods(&mut self, type_name: String, methods: Vec<String>) {
        let resolved_type = self.resolve_typedef(&type_name);
        self.type_methods.insert(resolved_type, methods);
    }

    fn get_all_methods(&self, type_name: &str) -> Vec<String> {
        let resolved_type = self.resolve_typedef(type_name);
        let mut all_methods = std::collections::HashSet::new();
        let mut visited = std::collections::HashSet::new();

        self.collect_all_methods(&resolved_type, &mut all_methods, &mut visited);
        all_methods.into_iter().collect()
    }
}

impl CInheritanceResolver {
    /// Build composition chain (similar to inheritance chain but for struct composition)
    fn build_composition_chain(
        &self,
        type_name: &str,
        chain: &mut Vec<String>,
        visited: &mut std::collections::HashSet<String>,
    ) {
        let resolved_type = self.resolve_typedef(type_name);

        if visited.contains(&resolved_type) {
            return; // Avoid infinite loops
        }
        visited.insert(resolved_type.clone());

        if let Some(composed_types) = self.composition_map.get(&resolved_type) {
            for (composed_type, _kind) in composed_types {
                chain.push(composed_type.clone());
                self.build_composition_chain(composed_type, chain, visited);
            }
        }
    }

    /// Check if child type is composed of (contains) parent type
    fn is_composed_of_recursive(
        &self,
        child: &str,
        parent: &str,
        visited: &mut std::collections::HashSet<String>,
    ) -> bool {
        if visited.contains(child) {
            return false; // Avoid infinite loops
        }
        visited.insert(child.to_string());

        if let Some(composed_types) = self.composition_map.get(child) {
            for (composed_type, _kind) in composed_types {
                if composed_type == parent {
                    return true;
                }
                if self.is_composed_of_recursive(composed_type, parent, visited) {
                    return true;
                }
            }
        }

        false
    }

    /// Collect all methods including those from composed types
    fn collect_all_methods(
        &self,
        type_name: &str,
        all_methods: &mut std::collections::HashSet<String>,
        visited: &mut std::collections::HashSet<String>,
    ) {
        if visited.contains(type_name) {
            return; // Avoid infinite loops
        }
        visited.insert(type_name.to_string());

        // Add methods defined directly for this type
        if let Some(methods) = self.type_methods.get(type_name) {
            all_methods.extend(methods.iter().cloned());
        }

        // Add methods from composed types
        if let Some(composed_types) = self.composition_map.get(type_name) {
            for (composed_type, _kind) in composed_types {
                self.collect_all_methods(composed_type, all_methods, visited);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_c_resolution_basic() {
        let file_id = FileId::new(1).unwrap();
        let mut context = CResolutionContext::new(file_id);
        let symbol_id = SymbolId::new(1).unwrap();

        // Add module-level symbol
        context.add_symbol("test_func".to_string(), symbol_id, ScopeLevel::Module);

        // Should resolve
        assert_eq!(context.resolve("test_func"), Some(symbol_id));

        // Should not resolve unknown symbol
        assert_eq!(context.resolve("unknown"), None);
    }

    #[test]
    fn test_scope_precedence() {
        let file_id = FileId::new(1).unwrap();
        let mut context = CResolutionContext::new(file_id);
        let local_id = SymbolId::new(1).unwrap();
        let module_id = SymbolId::new(2).unwrap();

        // Add same name at different levels
        context.add_symbol("name".to_string(), module_id, ScopeLevel::Module);
        context.add_symbol("name".to_string(), local_id, ScopeLevel::Local);

        // Local should take precedence
        assert_eq!(context.resolve("name"), Some(local_id));
    }

    #[test]
    fn test_scope_management() {
        let file_id = FileId::new(1).unwrap();
        let mut context = CResolutionContext::new(file_id);
        let symbol_id = SymbolId::new(1).unwrap();

        // Add local symbol
        context.add_symbol("local_var".to_string(), symbol_id, ScopeLevel::Local);
        assert_eq!(context.resolve("local_var"), Some(symbol_id));

        // Enter and exit function scope
        context.enter_scope(ScopeType::Function { hoisting: false });
        context.exit_scope();

        // Local scope should be cleared after exiting function
        assert_eq!(context.resolve("local_var"), None);
    }
}
