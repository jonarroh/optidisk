//! optidisk: escáner de disco paralelo y explorador TUI para liberar espacio.

pub mod app;
pub mod delete;
pub mod fmt;
pub mod model;
pub mod scan;
pub mod ui;

/// Pila de los hilos que recorren el árbol de forma recursiva: una ruta de Windows
/// admite miles de niveles, muchos más de los que caben en la pila por defecto.
pub const DEEP_STACK: usize = 64 * 1024 * 1024;
