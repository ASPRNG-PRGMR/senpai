#pragma once
#include <memory>
#include <string>
#include <vector>
#include <cstdint>
#include <cstddef>
#include "rust/cxx.h"

namespace senpai_poppler {

// PageSize is a "shared struct" defined in the cxx bridge (ffi.rs); cxx
// generates its full C++ definition into ffi.rs.h. Forward declaration is
// enough here since it's only used as a return type in a *declaration* —
// shim.cpp (which defines the methods) includes ffi.rs.h and therefore
// sees the complete type.
struct PageSize;

/// Owns a fully-decoded RGBA8 page image. Exposed to Rust as an opaque
/// type with raw accessor methods — Rust copies the bytes into its own
/// Vec<u8> once, at the FFI boundary, rather than us trying to hand over
/// a growable container across languages.
class RenderedImage {
public:
    RenderedImage(uint32_t width, uint32_t height, std::vector<uint8_t> rgba);
    uint32_t width() const;
    uint32_t height() const;
    const uint8_t *data() const;
    size_t len() const;

private:
    uint32_t width_;
    uint32_t height_;
    std::vector<uint8_t> rgba_;
};

/// Thin RAII wrapper around poppler::document. Owns the document for the
/// lifetime of the Rust-side handle; nothing about poppler's own types
/// crosses the cxx bridge.
class PopplerDocument {
public:
    ~PopplerDocument();

    int page_count() const;
    rust::String get_title() const;
    rust::String get_author() const;

    PageSize page_size(int index) const;
    std::unique_ptr<RenderedImage> render_page(int index, double scale) const;
    rust::String page_text(int index) const;

    // Only constructible via open_document below.
    struct Impl;
    std::unique_ptr<Impl> impl;
    PopplerDocument(std::unique_ptr<Impl> impl);
};

/// Opens a document from raw bytes already sitting in memory (backed by
/// senpai-core's mmap'd file). Returns nullptr on failure; the Rust side
/// turns that into an EngineError rather than throwing across the FFI
/// boundary.
std::unique_ptr<PopplerDocument> open_document(const uint8_t *data, size_t len);

} // namespace senpai_poppler
