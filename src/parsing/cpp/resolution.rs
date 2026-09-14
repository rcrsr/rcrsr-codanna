//! C++-specific resolution implementation
//!
//! This module provides C++ language resolution following the same pattern
//! as Rust and TypeScript implementations, with additional C++-specific features.

use crate::parsing::resolution::ImportBinding;
use crate::parsing::{InheritanceResolver, ResolutionScope, ScopeLevel, ScopeType};
use crate::{FileId, SymbolId};
use std::collections::HashMap;

/// C++-specific resolution context implementing C++ scoping rules
///
/// C++ has complex scoping with namespaces, classes, and inheritance:
/// 1. Local scope (function parameters, local variables)
/// 2. Class scope (member functions, member variables)
/// 3. Namespace scope (nested namespaces)
/// 4. Global scope (file-level and external symbols)
pub struct CppResolutionContext {
    #[allow(dead_code)]
    file_id: FileId, // Will be used for file-specific resolution

    /// Local variables and parameters in current scope
    local_scope: HashMap<String, SymbolId>,

    /// File-level symbols (functions, classes, variables)
    module_symbols: HashMap<String, SymbolId>,

    /// Symbols from included headers and namespaces
    imported_symbols: HashMap<String, SymbolId>,

    /// Global symbols visible across the project
    global_symbols: HashMap<String, SymbolId>,

    /// Track nested scopes (namespaces, classes, functions, blocks, etc.)
    scope_stack: Vec<ScopeType>,

    /// Include tracking (header paths)
    includes: Vec<String>,

    /// Using directives (using namespace ...)
    using_directives: Vec<String>,

    /// Using declarations (using std::vector)
    using_declarations: HashMap<String, SymbolId>,

    /// Class inheritance relationships (derived -> base classes)
    inheritance_graph: HashMap<SymbolId, Vec<SymbolId>>,

    /// Binding info for imports keyed by visible name
    import_bindings: HashMap<String, ImportBinding>,
}

impl CppResolutionContext {
    pub fn new(file_id: FileId) -> Self {
        Self {
            file_id,
            local_scope: HashMap::new(),
            module_symbols: HashMap::new(),
            imported_symbols: HashMap::new(),
            global_symbols: HashMap::new(),
            scope_stack: Vec::new(),
            includes: Vec::new(),
            using_directives: Vec::new(),
            using_declarations: HashMap::new(),
            inheritance_graph: HashMap::new(),
            import_bindings: HashMap::new(),
        }
    }

    /// Add an include directive
    pub fn add_include(&mut self, header_path: String) {
        self.includes.push(header_path);
    }

    /// Add using directive (using namespace ...)
    pub fn add_using_directive(&mut self, namespace: String) {
        self.using_directives.push(namespace);
    }

    /// Add using declaration (using std::vector)
    pub fn add_using_declaration(&mut self, name: String, symbol_id: SymbolId) {
        self.using_declarations.insert(name, symbol_id);
    }

    /// Add inheritance relationship
    pub fn add_inheritance(&mut self, derived: SymbolId, base: SymbolId) {
        self.inheritance_graph
            .entry(derived)
            .or_default()
            .push(base);
    }

    /// Check if one class derives from another
    pub fn derives_from(&self, derived: SymbolId, base: SymbolId) -> bool {
        let mut to_check = vec![derived];
        let mut visited = std::collections::HashSet::new();

        while let Some(current) = to_check.pop() {
            if visited.contains(&current) {
                continue;
            }
            visited.insert(current);

            if current == base {
                return true;
            }

            if let Some(bases) = self.inheritance_graph.get(&current) {
                to_check.extend(bases);
            }
        }

        false
    }

    /// Get base classes for a class
    pub fn get_base_classes(&self, class_id: SymbolId) -> Vec<SymbolId> {
        self.inheritance_graph
            .get(&class_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Get include list for this file
    pub fn includes(&self) -> &[String] {
        &self.includes
    }

    /// Add a local variable or parameter to the current scope
    pub fn add_local(&mut self, name: String, symbol_id: SymbolId) {
        self.local_scope.insert(name, symbol_id);
    }

    /// Add a file-level symbol (function, class, variable, namespace)
    pub fn add_module_symbol(&mut self, name: String, symbol_id: SymbolId) {
        self.module_symbols.insert(name, symbol_id);
    }

    /// Add an imported symbol from a header file or namespace
    pub fn add_import_symbol(&mut self, name: String, symbol_id: SymbolId) {
        self.imported_symbols.insert(name, symbol_id);
    }

    /// Add a global symbol visible project-wide
    pub fn add_global_symbol(&mut self, name: String, symbol_id: SymbolId) {
        self.global_symbols.insert(name, symbol_id);
    }
}

impl ResolutionScope for CppResolutionContext {
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
                // In C++, Package level maps to imported symbols from headers/namespaces
                self.imported_symbols.insert(name, symbol_id);
            }
            ScopeLevel::Global => {
                // Global symbols visible across the project
                self.global_symbols.insert(name, symbol_id);
            }
        }
    }

    fn resolve(&self, name: &str) -> Option<SymbolId> {
        // C++ resolution order: using declarations → local → module → imported → global

        // 1. Check using declarations first (highest priority)
        if let Some(&id) = self.using_declarations.get(name) {
            return Some(id);
        }

        // 2. Check local scope
        if let Some(&id) = self.local_scope.get(name) {
            return Some(id);
        }

        // 3. Check file-level symbols
        if let Some(&id) = self.module_symbols.get(name) {
            return Some(id);
        }

        // 4. Check imported symbols from headers/namespaces
        if let Some(&id) = self.imported_symbols.get(name) {
            return Some(id);
        }

        // 5. Check global symbols
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
        // C++ doesn't have JavaScript-style hoisting, so entering a scope doesn't affect resolution
    }

    fn exit_scope(&mut self) {
        self.scope_stack.pop();
        // Clear locals when exiting function scope
        if matches!(
            self.scope_stack.last(),
            None | Some(ScopeType::Module | ScopeType::Global | ScopeType::Namespace)
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
        // Store raw import paths (header files and using directives in C++)
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

/// Implementation of InheritanceResolver for C++
///
/// Handles C++ inheritance patterns including:
/// - Single and multiple inheritance
/// - Virtual inheritance
/// - Method overriding and hiding
/// - Access specifiers (private, protected, public inheritance)
pub struct CppInheritanceResolver {
    /// Maps child class name -> list of (parent class name, inheritance kind)
    /// kind can be "public", "protected", "private", "virtual public", etc.
    inheritance_map: HashMap<String, Vec<(String, String)>>,

    /// Maps type name -> methods defined directly in that type
    type_methods: HashMap<String, Vec<String>>,
}

impl CppInheritanceResolver {
    pub fn new() -> Self {
        Self {
            inheritance_map: HashMap::new(),
            type_methods: HashMap::new(),
        }
    }
}

impl Default for CppInheritanceResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl InheritanceResolver for CppInheritanceResolver {
    fn add_inheritance(&mut self, child: String, parent: String, kind: &str) {
        self.inheritance_map
            .entry(child)
            .or_default()
            .push((parent, kind.to_string()));
    }

    fn resolve_method(&self, type_name: &str, method: &str) -> Option<String> {
        // Check if the type itself defines the method
        if let Some(methods) = self.type_methods.get(type_name)
            && methods.contains(&method.to_string())
        {
            return Some(type_name.to_string());
        }

        // Search through inheritance chain using depth-first search
        // C++ method resolution follows the declaration order of base classes
        if let Some(parents) = self.inheritance_map.get(type_name) {
            for (parent_name, _inheritance_kind) in parents {
                if let Some(provider) = self.resolve_method(parent_name, method) {
                    return Some(provider);
                }
            }
        }

        None
    }

    fn get_inheritance_chain(&self, type_name: &str) -> Vec<String> {
        let mut chain = Vec::new();
        let mut visited = std::collections::HashSet::new();

        self.build_inheritance_chain(type_name, &mut chain, &mut visited);
        chain
    }

    fn is_subtype(&self, child: &str, parent: &str) -> bool {
        if child == parent {
            return true;
        }

        let mut visited = std::collections::HashSet::new();
        self.is_subtype_recursive(child, parent, &mut visited)
    }

    fn add_type_methods(&mut self, type_name: String, methods: Vec<String>) {
        self.type_methods.insert(type_name, methods);
    }

    fn get_all_methods(&self, type_name: &str) -> Vec<String> {
        let mut all_methods = std::collections::HashSet::new();
        let mut visited = std::collections::HashSet::new();

        self.collect_all_methods(type_name, &mut all_methods, &mut visited);
        all_methods.into_iter().collect()
    }
}

impl CppInheritanceResolver {
    /// Recursively build inheritance chain
    fn build_inheritance_chain(
        &self,
        type_name: &str,
        chain: &mut Vec<String>,
        visited: &mut std::collections::HashSet<String>,
    ) {
        if visited.contains(type_name) {
            return; // Avoid infinite loops
        }
        visited.insert(type_name.to_string());

        if let Some(parents) = self.inheritance_map.get(type_name) {
            for (parent_name, _inheritance_kind) in parents {
                chain.push(parent_name.clone());
                self.build_inheritance_chain(parent_name, chain, visited);
            }
        }
    }

    /// Recursively check if child is a subtype of parent
    fn is_subtype_recursive(
        &self,
        child: &str,
        parent: &str,
        visited: &mut std::collections::HashSet<String>,
    ) -> bool {
        if visited.contains(child) {
            return false; // Avoid infinite loops
        }
        visited.insert(child.to_string());

        if let Some(parents) = self.inheritance_map.get(child) {
            for (parent_name, _inheritance_kind) in parents {
                if parent_name == parent {
                    return true;
                }
                if self.is_subtype_recursive(parent_name, parent, visited) {
                    return true;
                }
            }
        }

        false
    }

    /// Recursively collect all methods including inherited ones
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

        // Add methods defined directly in this type
        if let Some(methods) = self.type_methods.get(type_name) {
            all_methods.extend(methods.iter().cloned());
        }

        // Add methods from parent classes
        // Note: In C++, derived class methods hide base class methods with the same name
        // This implementation adds all methods, but in a real implementation you'd need
        // to handle method hiding and overriding properly
        if let Some(parents) = self.inheritance_map.get(type_name) {
            for (parent_name, _inheritance_kind) in parents {
                self.collect_all_methods(parent_name, all_methods, visited);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cpp_resolution_basic() {
        let file_id = FileId::new(1).unwrap();
        let mut context = CppResolutionContext::new(file_id);
        let symbol_id = SymbolId::new(1).unwrap();

        // Add module-level symbol
        context.add_symbol("TestClass".to_string(), symbol_id, ScopeLevel::Module);

        // Should resolve
        assert_eq!(context.resolve("TestClass"), Some(symbol_id));

        // Should not resolve unknown symbol
        assert_eq!(context.resolve("unknown"), None);
    }

    #[test]
    fn test_using_declarations() {
        let file_id = FileId::new(1).unwrap();
        let mut context = CppResolutionContext::new(file_id);
        let vector_id = SymbolId::new(1).unwrap();
        let local_id = SymbolId::new(2).unwrap();

        // Add local symbol with same name
        context.add_symbol("vector".to_string(), local_id, ScopeLevel::Local);

        // Add using declaration
        context.add_using_declaration("vector".to_string(), vector_id);

        // Using declaration should take precedence
        assert_eq!(context.resolve("vector"), Some(vector_id));
    }

    #[test]
    fn test_inheritance_tracking() {
        let file_id = FileId::new(1).unwrap();
        let mut context = CppResolutionContext::new(file_id);

        let base_id = SymbolId::new(1).unwrap();
        let derived_id = SymbolId::new(2).unwrap();
        let derived2_id = SymbolId::new(3).unwrap();

        // Set up inheritance: derived -> base, derived2 -> derived
        context.add_inheritance(derived_id, base_id);
        context.add_inheritance(derived2_id, derived_id);

        // Test direct inheritance
        assert!(context.derives_from(derived_id, base_id));

        // Test transitive inheritance
        assert!(context.derives_from(derived2_id, base_id));

        // Test non-inheritance
        assert!(!context.derives_from(base_id, derived_id));
    }

    #[test]
    fn test_scope_management() {
        let file_id = FileId::new(1).unwrap();
        let mut context = CppResolutionContext::new(file_id);
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
