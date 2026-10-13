# Contributing to Codanna

Thank you for your interest in contributing to Codanna! This guide focuses on the development workflow and contributor-specific requirements.

## Quick Links

- **[Development Guidelines](./development/guidelines.md)** - Rust coding principles (MUST READ)
- **[Adding Language Support](./development/language-support.md)** - Complete language implementation guide
- **[CI Local/Remote Parity](./development/ci-local-remote-parity.md)** - Ensuring local and remote CI match
- **[Development Setup](#development-setup)** - Local environment setup
- **[Testing Workflow](#testing-your-changes)** - Pre-commit and CI/CD scripts

## Current Status

See [CHANGELOG.md](../CHANGELOG.md) for detailed release notes and feature history.

**Stable Architecture** - Language registry, resolution API, and signature extraction are production-ready
**5 Languages Supported** - Rust, TypeScript, Python, Go, PHP, C, C++ with comprehensive feature parity
**Ready for New Languages** - Mature, well-tested architecture for easy expansion

## Development Setup

### Prerequisites

- Rust 1.75+ (install via [rustup](https://rustup.rs/))
- Git

### System Dependencies

**Linux (Ubuntu/Debian):**
```bash
sudo apt update && sudo apt install pkg-config
```

**Linux (CentOS/RHEL):**
```bash
sudo yum install pkgconfig
```

**Linux (Fedora):**
```bash
sudo dnf install pkgconfig
```

### Getting Started

1. **Fork and clone the repository:**
   ```bash
   git clone https://github.com/bartolli/codanna.git
   cd codanna
   ```

2. **Build the project:**
   ```bash
   cargo build --release --all-features
   ```

3. **Run tests:**
   ```bash
   cargo test
   ```

4. **Set up pre-commit checks:**
   ```bash
   # Make scripts executable
   chmod +x contributing/scripts/*.sh

   # Run quick checks before committing
   ./contributing/scripts/quick-check.sh
   ```

### Recommended: Codanna Plugin for Claude Code

If you use Claude Code, install the Codanna plugin for better code navigation:

```bash
# Add the Codanna marketplace
/plugin marketplace add bartolli/codanna-plugins

# Install the plugin
/plugin install codanna-cc@codanna-plugins

# Navigate the codebase with /x-ray
/codanna-cc:x-ray "How does symbol resolution work?"
/codanna-cc:x-ray "Where is JSX component tracking implemented?"

# Look up specific symbols
/codanna-cc:symbol TypeScriptParser
```

The plugin indexes this codebase and provides semantic search, making it easier to understand the architecture and find implementation details.

## Project Structure

```
codanna/
├── src/
│   ├── parsing/         # Language parsers (rust/, typescript/, python/, php/, go/)
│   ├── indexing/        # Symbol indexing and resolution
│   ├── storage/         # Tantivy and memory-mapped caches
│   └── mcp/            # MCP server and HTTP/HTTPS endpoints
├── contributing/        # Development tools and documentation
└── tests/              # Language parser and integration tests
```

## Development Tools

### Parse Command

The `codanna parse` command is essential for parser development and debugging:

```bash
# Parse a file and output AST nodes in JSONL format
codanna parse file.rs                      # Named nodes only (like tree-sitter)
codanna parse file.rs --all-nodes          # Include all nodes (punctuation, keywords)
codanna parse file.rs --max-depth 3        # Limit traversal depth
codanna parse file.rs -o ast.jsonl         # Output to file

# Analyze AST structure
codanna parse file.rs | jq -r .node | sort -u     # List unique node types
codanna parse file.rs | jq 'select(.depth == 1)'  # Show top-level nodes
codanna parse file.rs | jq 'select(.node == "function_item")'  # Find specific nodes
```

**Key Features:**
- **Default behavior matches tree-sitter CLI** - Shows only named nodes for direct comparison
- **`--all-nodes` flag** - Shows complete AST including anonymous nodes (operators, punctuation)
- **JSONL format** - One JSON object per line, perfect for streaming and Unix tools
- **Hierarchy tracking** - Each node includes depth, parent ID, and unique ID
- **Error codes** - Proper exit codes (3=NotFound, 4=ParseError, 8=UnsupportedLanguage)

### Tree-sitter Integration Scripts

Located in `contributing/tree-sitter/scripts/`:

#### setup.sh
Install tree-sitter grammars for testing:
```bash
./contributing/tree-sitter/scripts/setup.sh typescript  # Install specific grammar
./contributing/tree-sitter/scripts/setup.sh            # Show installed grammars
```

#### compare-nodes.sh
Compare codanna parser with tree-sitter (two modes):

**Language mode** - Runs audit tests and generates reports:
```bash
./contributing/tree-sitter/scripts/compare-nodes.sh rust
```
This mode:
- Runs `cargo test comprehensive_rust_analysis`
- Generates audit reports in `contributing/parsers/rust/`:
  - `AUDIT_REPORT.md` - Parser coverage analysis
  - `GRAMMAR_ANALYSIS.md` - Node handling statistics
- Compares comprehensive example files
- Shows parser implementation gaps

**File mode** - Compares any specific file:
```bash
./contributing/tree-sitter/scripts/compare-nodes.sh examples/rust/main.rs
```
This mode:
- Uses `codanna parse` to analyze the file
- Compares with tree-sitter output
- Saves detailed comparison to `{filename}_comparison.log`
- Shows matching statistics

#### explore-ast.sh
Quick AST exploration:
```bash
# Use codanna (default)
./contributing/tree-sitter/scripts/explore-ast.sh file.rs

# Use tree-sitter
./contributing/tree-sitter/scripts/explore-ast.sh file.rs tree-sitter

# Compare both
./contributing/tree-sitter/scripts/explore-ast.sh file.rs both
```

## Testing Your Changes

We provide local scripts that replicate our CI/CD pipeline:

### 1. Quick Checks (2-3 minutes)
Run before every commit to catch common issues:
```bash
./contributing/scripts/quick-check.sh
```
This runs:
- Format checking (cargo fmt --check)
- Clippy linting (cargo clippy)
- Compile check (cargo check --all-features)

### 2. Auto-Fix Issues
Automatically fix formatting and linting issues:
```bash
./contributing/scripts/auto-fix.sh
```
This will:
- Format your code (cargo fmt)
- Fix clippy issues where possible
- Run quick-check to verify fixes

### 3. Full Test Suite
Run before submitting PR to ensure all tests pass:
```bash
./contributing/scripts/full-test.sh
```
This replicates the complete GitHub Actions workflow.

## Development Guidelines

**IMPORTANT**: All code must follow our [Rust Development Guidelines](./development/guidelines.md). Key principles:

1. **Zero-Cost Abstractions**: No unnecessary allocations
2. **Type Safety**: Use newtypes, not primitives
3. **Performance**: Must meet performance targets
4. **Error Handling**: Structured errors with suggestions
5. **Function Design**: Decompose complex logic into focused helper methods

### Code Style

- Use `cargo fmt` for formatting
- Fix all `cargo clippy` warnings
- Write tests for new features
- Document public APIs with examples

### Performance Requirements

- Parser speed: >10,000 symbols/second (AST parsing only)
- Symbol lookups: <10ms from memory-mapped cache
- Semantic search: <300ms end-to-end (including embedding generation)
- Memory usage: ~100 bytes per symbol in cache
- CLI startup: <500ms for all operations

## Adding New Features

### Language Support

See [Adding Language Support](./development/language-support.md) for the complete guide. Critical requirements:

- [ ] **6 required files** in `src/parsing/{language}/` directory
- [ ] **Complete signature extraction** for all symbol types
- [ ] **Language-specific resolution logic** in resolution.rs
- [ ] **Registry registration** and tree-sitter dependency
- [ ] **Comprehensive test coverage** with ABI-15 exploration

### New Commands

> You are free to add any command you find useful for your workflow. However, if you plan to make a PR, please open an Issue firs, outline the problem the feature aims to solve and let's discuss it.

1. Add command to CLI enum in `src/main.rs`
2. Implement handler function
3. Add tests
4. Update README.md with usage examples
5. Support `--json` flag for structured output

## Submitting Pull Requests

### Before Submitting

1. **Run all local checks:**
   ```bash
   ./contributing/scripts/auto-fix.sh
   ./contributing/scripts/full-test.sh
   ```

2. **Update documentation:**
   - Add/update relevant documentation
   - Include usage examples
   - Update README.md if adding features

3. **Write good commit messages:**
   ```
   feat: Add Go language parser support

   - Implement LanguageParser and LanguageBehavior traits
   - Add complete signature extraction for all symbol types
   - Support structs, interfaces, functions, and packages
   - Parse >75,000 symbols/second with scope tracking
   ```

### PR Guidelines

1. **Start with an issue**:
   - Create an issue describing your proposed change
   - Wait for feedback before implementing major features
   - Link your PR to the issue with "Fixes #123"
2. **Scope your PRs appropriately**:
   - One feature/major change per PR
   - Multiple small related fixes can be combined
   - Keep PRs focused and reviewable
3. **Include tests** - All new code needs tests
4. **Performance impact** - Document any performance changes
5. **Breaking changes** - Clearly mark if API changes
6. **Screenshots/examples** - Show the feature in action

### PR Template

```markdown
## Description
Brief description of changes

## Type of Change
- [ ] Bug fix
- [ ] New feature
- [ ] Breaking change
- [ ] Documentation update

## Testing
- [ ] Tests pass locally
- [ ] Added new tests
- [ ] Performance verified

## Checklist
- [ ] Code follows guidelines
- [ ] Self-reviewed
- [ ] Documentation updated
- [ ] No new warnings
```

## Getting Help

- **Issues**: Check existing [issues](https://github.com/bartolli/codanna/issues) or create new ones
- **Discussions**: Use GitHub Discussions for questions
- **Documentation**: See `/docs` folder for detailed documentation
- **Before implementing**: Create an issue first to discuss your proposed changes

## Code of Conduct

- Be respectful and inclusive
- Focus on constructive feedback
- Assume good intentions

## Recognition

Contributors are recognized in:
- GitHub contributors page

We're in an era where AI agents are getting smarter and need scalable, fast, and precise context on demand. Context integration matters.

## License

By contributing, you agree that your contributions will be licensed under the Apache License 2.0.
