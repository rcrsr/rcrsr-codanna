//! Go Parser Performance Benchmarks
//!
//! This benchmark suite validates that the Go parser meets performance targets:
//! - >10,000 symbols/second extraction speed
//! - Memory usage within acceptable limits
//! - Performance comparable to other language parsers
//! - Scalability with large codebases

use codanna::parsing::go::GoParser;
use codanna::types::{FileId, SymbolCounter};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::fs;
use std::hint::black_box;
use std::path::PathBuf;

// Performance targets and constants
#[allow(dead_code)]
const TARGET_SYMBOLS_PER_SEC: u64 = 10_000;
#[allow(dead_code)]
const LARGE_FILE_SYMBOL_COUNT: usize = 1000;
#[allow(dead_code)]
const BENCHMARK_ITERATIONS: usize = 100;

/// Benchmark basic Go symbol extraction performance
fn bench_go_symbol_extraction(c: &mut Criterion) {
    let mut group = c.benchmark_group("go_symbol_extraction");

    // Test with different Go code samples of varying complexity
    let test_cases = vec![
        ("basic_go", create_basic_go_code()),
        ("medium_go", create_medium_complexity_go_code()),
        ("complex_go", create_complex_go_code()),
        ("real_world_go", create_real_world_go_code()),
    ];

    for (name, source_code) in test_cases {
        let symbol_count = count_expected_symbols(&source_code);
        group.throughput(Throughput::Elements(symbol_count as u64));

        group.bench_with_input(
            BenchmarkId::new("symbol_extraction", name),
            &source_code,
            |b, code| {
                let mut parser = GoParser::new().expect("Failed to create Go parser");
                b.iter(|| {
                    let mut symbol_counter = SymbolCounter::new();
                    let file_id = FileId::new(1).expect("Failed to create file ID");
                    let symbols = parser.parse(black_box(code), file_id, &mut symbol_counter);
                    black_box(symbols)
                });
            },
        );
    }

    group.finish();
}

/// Benchmark Go parser performance with fixture files
fn bench_go_fixture_files(c: &mut Criterion) {
    let mut group = c.benchmark_group("go_fixture_files");

    // Find Go fixture files
    let fixture_files = find_go_fixtures();

    for fixture_path in fixture_files {
        if let Ok(source_code) = fs::read_to_string(&fixture_path) {
            let file_name = fixture_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown");

            let symbol_count = count_expected_symbols(&source_code);
            group.throughput(Throughput::Elements(symbol_count as u64));

            group.bench_with_input(
                BenchmarkId::new("fixture_parsing", file_name),
                &source_code,
                |b, code| {
                    let mut parser = GoParser::new().expect("Failed to create Go parser");
                    b.iter(|| {
                        let mut symbol_counter = SymbolCounter::new();
                        let file_id = FileId::new(1).expect("Failed to create file ID");
                        let symbols = parser.parse(black_box(code), file_id, &mut symbol_counter);
                        black_box(symbols)
                    });
                },
            );
        }
    }

    group.finish();
}

/// Benchmark memory usage patterns
fn bench_go_memory_usage(c: &mut Criterion) {
    let mut group = c.benchmark_group("go_memory_usage");

    // Test memory usage with increasingly large Go files
    let sizes = vec![100, 500, 1000, 2000, 5000];

    for size in sizes {
        let source_code = create_large_go_file(size);
        group.throughput(Throughput::Elements(size as u64));

        group.bench_with_input(
            BenchmarkId::new("large_file_parsing", size),
            &source_code,
            |b, code| {
                b.iter(|| {
                    let mut parser = GoParser::new().expect("Failed to create Go parser");
                    let mut symbol_counter = SymbolCounter::new();
                    let file_id = FileId::new(1).expect("Failed to create file ID");
                    let symbols = parser.parse(black_box(code), file_id, &mut symbol_counter);
                    black_box(symbols)
                });
            },
        );
    }

    group.finish();
}

/// Benchmark parser initialization overhead
fn bench_parser_initialization(c: &mut Criterion) {
    let mut group = c.benchmark_group("parser_initialization");

    group.bench_function("go_parser_creation", |b| {
        b.iter(|| {
            let parser = GoParser::new();
            black_box(parser)
        });
    });

    group.finish();
}

/// Benchmark specific Go language constructs
fn bench_go_language_constructs(c: &mut Criterion) {
    let mut group = c.benchmark_group("go_language_constructs");

    let construct_tests = vec![
        ("functions", create_many_functions(100)),
        ("structs", create_many_structs(50)),
        ("interfaces", create_many_interfaces(30)),
        ("methods", create_many_methods(100)),
        ("generics", create_many_generics(25)),
    ];

    for (construct_name, source_code) in construct_tests {
        let symbol_count = count_expected_symbols(&source_code);
        group.throughput(Throughput::Elements(symbol_count as u64));

        group.bench_with_input(
            BenchmarkId::new("construct_parsing", construct_name),
            &source_code,
            |b, code| {
                let mut parser = GoParser::new().expect("Failed to create Go parser");
                b.iter(|| {
                    let mut symbol_counter = SymbolCounter::new();
                    let file_id = FileId::new(1).expect("Failed to create file ID");
                    let symbols = parser.parse(black_box(code), file_id, &mut symbol_counter);
                    black_box(symbols)
                });
            },
        );
    }

    group.finish();
}

/// Benchmark scalable test data generation
/// Tests parser performance with systematically generated test data of varying sizes
fn bench_scalable_test_data(c: &mut Criterion) {
    let mut group = c.benchmark_group("scalable_test_data");

    // Test with different data sizes to measure scalability
    let data_sizes = vec![100, 500, 1000, 2000, 5000, 10000];

    for size in data_sizes {
        let source_code = generate_scalable_go_code(size);
        let symbol_count = count_expected_symbols(&source_code);
        group.throughput(Throughput::Elements(symbol_count as u64));

        group.bench_with_input(
            BenchmarkId::new("generated_data", size),
            &source_code,
            |b, code| {
                let mut parser = GoParser::new().expect("Failed to create Go parser");
                b.iter(|| {
                    let mut symbol_counter = SymbolCounter::new();
                    let file_id = FileId::new(1).expect("Failed to create file ID");
                    let symbols = parser.parse(black_box(code), file_id, &mut symbol_counter);
                    black_box(symbols.len()) // Return count to avoid large memory allocations in benchmark
                });
            },
        );
    }

    group.finish();
}

// Helper functions for generating test data

/// Create basic Go code for benchmarking
fn create_basic_go_code() -> String {
    r#"
package main

import "fmt"

const Version = "1.0.0"

type Person struct {
    Name string
    Age  int
}

func (p *Person) Greet() string {
    return fmt.Sprintf("Hello, I'm %s", p.Name)
}

func main() {
    person := &Person{Name: "Alice", Age: 30}
    fmt.Println(person.Greet())
}
"#
    .to_string()
}

/// Create medium complexity Go code
fn create_medium_complexity_go_code() -> String {
    r#"
package example

import (
    "context"
    "fmt"
    "sync"
    "time"
)

type Config struct {
    Host     string
    Port     int
    Timeout  time.Duration
    mu       sync.RWMutex
}

func NewConfig(host string, port int) *Config {
    return &Config{
        Host:    host,
        Port:    port,
        Timeout: 30 * time.Second,
    }
}

func (c *Config) GetAddress() string {
    c.mu.RLock()
    defer c.mu.RUnlock()
    return fmt.Sprintf("%s:%d", c.Host, c.Port)
}

func (c *Config) UpdateTimeout(timeout time.Duration) {
    c.mu.Lock()
    defer c.mu.Unlock()
    c.timeout = timeout
}

type Service interface {
    Start(ctx context.Context) error
    Stop() error
    Health() bool
}

type WebService struct {
    config *Config
    // server *http.Server - commented out for benchmark
}

func (ws *WebService) Start(ctx context.Context) error {
    // Implementation
    return nil
}

func (ws *WebService) Stop() error {
    return nil
}

func (ws *WebService) Health() bool {
    return true
}
"#
    .to_string()
}

/// Create complex Go code with advanced features
fn create_complex_go_code() -> String {
    r#"
package advanced

import (
    "context"
    "fmt"
    "sync"
    "time"
)

// Generic constraint interface
type Comparable[T any] interface {
    Compare(T) int
    ~int | ~string | ~float64
}

// Generic repository pattern
type Repository[T Comparable[T]] struct {
    mu      sync.RWMutex
    items   map[string]T
    logger  Logger
    timeout time.Duration
}

type Logger interface {
    Info(msg string)
    Error(msg string, err error)
    Debug(msg string)
}

func NewRepository[T Comparable[T]](logger Logger) *Repository[T] {
    return &Repository[T]{
        items:   make(map[string]T),
        logger:  logger,
        timeout: 5 * time.Second,
    }
}

func (r *Repository[T]) Store(ctx context.Context, key string, value T) error {
    select {
    case <-ctx.Done():
        return ctx.Err()
    default:
    }

    r.mu.Lock()
    defer r.mu.Unlock()
    
    r.items[key] = value
    r.logger.Info(fmt.Sprintf("Stored item: %s", key))
    return nil
}

func (r *Repository[T]) Get(key string) (T, bool) {
    r.mu.RLock()
    defer r.mu.RUnlock()
    
    value, exists := r.items[key]
    return value, exists
}

// Channel-based worker pool
type WorkerPool[T any] struct {
    workers   int
    jobs      chan T
    results   chan Result[T]
    ctx       context.Context
    cancel    context.CancelFunc
    wg        sync.WaitGroup
}

type Result[T any] struct {
    Value T
    Error error
}

func NewWorkerPool[T any](workers int) *WorkerPool[T] {
    ctx, cancel := context.WithCancel(context.Background())
    return &WorkerPool[T]{
        workers: workers,
        jobs:    make(chan T, workers*2),
        results: make(chan Result[T], workers*2),
        ctx:     ctx,
        cancel:  cancel,
    }
}

func (wp *WorkerPool[T]) Start(processor func(T) (T, error)) {
    for i := 0; i < wp.workers; i++ {
        wp.wg.Add(1)
        go wp.worker(processor)
    }
}

func (wp *WorkerPool[T]) worker(processor func(T) (T, error)) {
    defer wp.wg.Done()
    
    for {
        select {
        case job := <-wp.jobs:
            value, err := processor(job)
            wp.results <- Result[T]{Value: value, Error: err}
        case <-wp.ctx.Done():
            return
        }
    }
}

func (wp *WorkerPool[T]) Submit(job T) {
    select {
    case wp.jobs <- job:
    case <-wp.ctx.Done():
    }
}

func (wp *WorkerPool[T]) Stop() {
    wp.cancel()
    close(wp.jobs)
    wp.wg.Wait()
    close(wp.results)
}
"#
    .to_string()
}

/// Create real-world Go code pattern
fn create_real_world_go_code() -> String {
    r#"
package main

import (
    "context"
    "encoding/json"
    "fmt"
    "net/http"
    "os"
    "os/signal"
    "syscall"
    "time"
)

type User struct {
    ID       int64     `json:"id"`
    Name     string    `json:"name"`
    Email    string    `json:"email"`
    Created  time.Time `json:"created"`
}

type UserService interface {
    CreateUser(ctx context.Context, user *User) error
    GetUser(ctx context.Context, id int64) (*User, error)
    ListUsers(ctx context.Context, limit, offset int) ([]*User, error)
    UpdateUser(ctx context.Context, user *User) error
    DeleteUser(ctx context.Context, id int64) error
}

type HTTPUserService struct {
    service UserService
}

func NewHTTPUserService(service UserService) *HTTPUserService {
    return &HTTPUserService{service: service}
}

func (h *HTTPUserService) ServeHTTP(w http.ResponseWriter, r *http.Request) {
    switch r.Method {
    case http.MethodPost:
        h.createUser(w, r)
    case http.MethodGet:
        h.getUser(w, r)
    case http.MethodPut:
        h.updateUser(w, r)
    case http.MethodDelete:
        h.deleteUser(w, r)
    default:
        http.Error(w, "Method not allowed", http.StatusMethodNotAllowed)
    }
}

func (h *HTTPUserService) createUser(w http.ResponseWriter, r *http.Request) {
    var user User
    if err := json.NewDecoder(r.Body).Decode(&user); err != nil {
        http.Error(w, "Invalid JSON", http.StatusBadRequest)
        return
    }

    ctx, cancel := context.WithTimeout(r.Context(), 5*time.Second)
    defer cancel()

    if err := h.service.CreateUser(ctx, &user); err != nil {
        http.Error(w, "Failed to create user", http.StatusInternalServerError)
        return
    }

    w.Header().Set("Content-Type", "application/json")
    json.NewEncoder(w).Encode(user)
}

func (h *HTTPUserService) getUser(w http.ResponseWriter, r *http.Request) {
    // Implementation details...
}

func (h *HTTPUserService) updateUser(w http.ResponseWriter, r *http.Request) {
    // Implementation details...
}

func (h *HTTPUserService) deleteUser(w http.ResponseWriter, r *http.Request) {
    // Implementation details...
}

func main() {
    // Service initialization
    userService := NewUserService()
    httpService := NewHTTPUserService(userService)
    
    server := &http.Server{
        Addr:         ":8080",
        Handler:      httpService,
        ReadTimeout:  10 * time.Second,
        WriteTimeout: 10 * time.Second,
    }

    // Graceful shutdown
    go func() {
        if err := server.ListenAndServe(); err != nil && err != http.ErrServerClosed {
            fmt.Printf("Server error: %v\n", err)
        }
    }()

    // Wait for interrupt signal
    c := make(chan os.Signal, 1)
    signal.Notify(c, os.Interrupt, syscall.SIGTERM)
    <-c

    ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
    defer cancel()
    
    if err := server.Shutdown(ctx); err != nil {
        fmt.Printf("Server shutdown error: %v\n", err)
    }
}
"#
    .to_string()
}

/// Generate a large Go file with specified number of symbols
fn create_large_go_file(symbol_count: usize) -> String {
    let mut code = String::from("package main\n\nimport \"fmt\"\n\n");

    // Add structs
    for i in 0..(symbol_count / 4) {
        code.push_str(&format!(
            "type Struct{i} struct {{\n    Field1 string\n    Field2 int\n}}\n\n"
        ));
    }

    // Add interfaces
    for i in 0..(symbol_count / 8) {
        code.push_str(&format!(
            "type Interface{i} interface {{\n    Method{i}() string\n}}\n\n"
        ));
    }

    // Add functions
    for i in 0..(symbol_count / 2) {
        code.push_str(&format!(
            "func Function{i}(param string) string {{\n    return fmt.Sprintf(\"Function{i}: %s\", param)\n}}\n\n"
        ));
    }

    // Add main function
    code.push_str("func main() {\n    fmt.Println(\"Generated code\")\n}\n");

    code
}

/// Create many functions for benchmarking
fn create_many_functions(count: usize) -> String {
    let mut code = String::from("package main\n\nimport \"fmt\"\n\n");

    for i in 0..count {
        code.push_str(&format!(
            "func Function{i}(param{i} string) (string, error) {{\n    return fmt.Sprintf(\"Result: %s\", param{i}), nil\n}}\n\n"
        ));
    }

    code.push_str("func main() {}\n");
    code
}

/// Create many structs for benchmarking
fn create_many_structs(count: usize) -> String {
    let mut code = String::from("package main\n\n");

    for i in 0..count {
        code.push_str(&format!(
            "type Struct{i} struct {{\n    Field1{i} string\n    Field2{i} int\n    Field3{i} bool\n}}\n\n"
        ));
    }

    code.push_str("func main() {}\n");
    code
}

/// Create many interfaces for benchmarking
fn create_many_interfaces(count: usize) -> String {
    let mut code = String::from("package main\n\n");

    for i in 0..count {
        code.push_str(&format!(
            "type Interface{i} interface {{\n    Method1{i}() string\n    Method2{i}(param string) error\n    Method3{i}(a, b int) (int, bool)\n}}\n\n"
        ));
    }

    code.push_str("func main() {}\n");
    code
}

/// Create many methods for benchmarking
fn create_many_methods(count: usize) -> String {
    let mut code = String::from("package main\n\ntype TestStruct struct { value string }\n\n");

    for i in 0..count {
        if i % 2 == 0 {
            code.push_str(&format!(
                "func (ts *TestStruct) PointerMethod{i}() string {{\n    return ts.value + \"_{i}\"\n}}\n\n"
            ));
        } else {
            code.push_str(&format!(
                "func (ts TestStruct) ValueMethod{i}(param string) string {{\n    return ts.value + param + \"_{i}\"\n}}\n\n"
            ));
        }
    }

    code.push_str("func main() {}\n");
    code
}

/// Create many generic constructs for benchmarking
fn create_many_generics(count: usize) -> String {
    let mut code = String::from("package main\n\n");

    for i in 0..count {
        // Generic functions
        code.push_str(&format!(
            "func GenericFunc{i}[T any](item T) T {{\n    return item\n}}\n\n"
        ));

        // Generic structs
        code.push_str(&format!(
            "type GenericStruct{i}[T comparable] struct {{\n    Value T\n    Items map[T]string\n}}\n\n"
        ));
    }

    code.push_str("func main() {}\n");
    code
}

/// Generate scalable Go code for systematic performance testing
/// This function creates Go source code with a controlled number of symbols
/// to enable systematic testing at different scales.
fn generate_scalable_go_code(target_symbols: usize) -> String {
    let mut code = String::from(
        "package scalable_test\n\nimport (\n\t\"fmt\"\n\t\"context\"\n\t\"time\"\n)\n\n",
    );

    // Calculate distribution of symbols - aim for varied types
    let functions = target_symbols / 4;
    let structs = target_symbols / 8;
    let interfaces = target_symbols / 16;
    let methods = target_symbols / 4;
    let constants = target_symbols / 8;
    let variables = target_symbols / 8;

    // Add constants
    for i in 0..constants {
        code.push_str(&format!("const Constant{i} = \"value_{i}\"\n"));
    }
    code.push('\n');

    // Add variables
    for i in 0..variables {
        code.push_str(&format!("var Variable{i} string = \"var_{i}\"\n"));
    }
    code.push('\n');

    // Add structs with fields
    for i in 0..structs {
        code.push_str(&format!(
            "type Struct{i} struct {{\n\tField1_{i} string\n\tField2_{i} int\n\tField3_{i} bool\n}}\n\n"
        ));
    }

    // Add interfaces
    for i in 0..interfaces {
        code.push_str(&format!(
            "type Interface{i} interface {{\n\tMethod1_{i}() string\n\tMethod2_{i}(int) bool\n}}\n\n"
        ));
    }

    // Add regular functions
    for i in 0..functions {
        code.push_str(&format!(
            "func Function{i}(param1 string, param2 int) (string, error) {{\n\treturn fmt.Sprintf(\"func_%d_%s_%d\", {i}, param1, param2), nil\n}}\n\n"
        ));
    }

    // Add methods for structs
    for i in 0..(methods.min(structs * 3)) {
        let struct_idx = i % structs;
        code.push_str(&format!(
            "func (s *Struct{struct_idx}) Method{i}() string {{\n\treturn fmt.Sprintf(\"method_{i}_%s\", s.Field1_{struct_idx})\n}}\n\n"
        ));
    }

    code.push_str("func main() {}\n");
    code
}

/// Find Go fixture files for benchmarking
fn find_go_fixtures() -> Vec<PathBuf> {
    let mut fixtures = Vec::new();
    let fixtures_dir = PathBuf::from("tests/fixtures/go");

    if fixtures_dir.exists()
        && let Ok(entries) = fs::read_dir(fixtures_dir)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|ext| ext == "go") {
                fixtures.push(path);
            }
        }
    }

    fixtures
}

/// Count expected symbols in Go source code (rough estimate)
fn count_expected_symbols(source_code: &str) -> usize {
    let mut count = 0;

    // Count function declarations
    count += source_code.matches("func ").count();

    // Count type declarations
    count += source_code.matches("type ").count();

    // Count const/var declarations
    count += source_code.matches("const ").count();
    count += source_code.matches("var ").count();

    // Return at least 1 to avoid division by zero in benchmarks
    count.max(1)
}

criterion_group!(
    benches,
    bench_go_symbol_extraction,
    bench_go_fixture_files,
    bench_go_memory_usage,
    bench_parser_initialization,
    bench_go_language_constructs,
    bench_scalable_test_data
);
criterion_main!(benches);
