#include "shim.h"
#include "senpai-pdf-poppler/src/ffi.rs.h"

#include <poppler-document.h>
#include <poppler-page.h>
#include <poppler-page-renderer.h>
#include <poppler-image.h>

namespace senpai_poppler {

RenderedImage::RenderedImage(uint32_t width, uint32_t height, std::vector<uint8_t> rgba)
    : width_(width), height_(height), rgba_(std::move(rgba)) {}

uint32_t RenderedImage::width() const { return width_; }
uint32_t RenderedImage::height() const { return height_; }
const uint8_t *RenderedImage::data() const { return rgba_.data(); }
size_t RenderedImage::len() const { return rgba_.size(); }

struct PopplerDocument::Impl {
    std::unique_ptr<poppler::document> doc;
};

PopplerDocument::PopplerDocument(std::unique_ptr<Impl> impl_) : impl(std::move(impl_)) {}
PopplerDocument::~PopplerDocument() = default;

int PopplerDocument::page_count() const {
    return impl->doc->pages();
}

rust::String PopplerDocument::get_title() const {
    return rust::String(impl->doc->get_title().to_latin1());
}

rust::String PopplerDocument::get_author() const {
    return rust::String(impl->doc->get_author().to_latin1());
}

PageSize PopplerDocument::page_size(int index) const {
    std::unique_ptr<poppler::page> p(impl->doc->create_page(index));
    if (!p) {
        return PageSize{0.0, 0.0};
    }
    poppler::rectf r = p->page_rect();
    return PageSize{r.width(), r.height()};
}

std::unique_ptr<RenderedImage> PopplerDocument::render_page(int index, double scale) const {
    std::unique_ptr<poppler::page> p(impl->doc->create_page(index));
    if (!p) {
        return std::make_unique<RenderedImage>(0, 0, std::vector<uint8_t>{});
    }

    // scale 1.0 == 72 DPI (1:1 with PDF points); poppler's renderer takes
    // resolution in DPI directly.
    double dpi = 72.0 * scale;

    poppler::page_renderer renderer;
    renderer.set_render_hint(poppler::page_renderer::antialiasing, true);
    renderer.set_render_hint(poppler::page_renderer::text_antialiasing, true);

    poppler::image img = renderer.render_page(p.get(), dpi, dpi);
    if (!img.is_valid()) {
        return std::make_unique<RenderedImage>(0, 0, std::vector<uint8_t>{});
    }

    uint32_t w = static_cast<uint32_t>(img.width());
    uint32_t h = static_cast<uint32_t>(img.height());
    std::vector<uint8_t> out(static_cast<size_t>(w) * h * 4);

    const char *src = img.data();
    int bpr = img.bytes_per_row();
    for (uint32_t y = 0; y < h; ++y) {
        const uint8_t *row = reinterpret_cast<const uint8_t *>(src + y * bpr);
        for (uint32_t x = 0; x < w; ++x) {
            // poppler's argb32 is stored byte order B,G,R,A on little-endian;
            // convert to straight R,G,B,A for our RenderedPage contract.
            size_t si = static_cast<size_t>(x) * 4;
            size_t di = (static_cast<size_t>(y) * w + x) * 4;
            out[di + 0] = row[si + 2]; // R
            out[di + 1] = row[si + 1]; // G
            out[di + 2] = row[si + 0]; // B
            out[di + 3] = row[si + 3]; // A
        }
    }

    return std::make_unique<RenderedImage>(w, h, std::move(out));
}

rust::String PopplerDocument::page_text(int index) const {
    std::unique_ptr<poppler::page> p(impl->doc->create_page(index));
    if (!p) {
        return rust::String("");
    }
    return rust::String(p->text().to_latin1());
}

std::unique_ptr<PopplerDocument> open_document(const uint8_t *data, size_t len) {
    auto impl = std::make_unique<PopplerDocument::Impl>();
    impl->doc.reset(poppler::document::load_from_raw_data(
        reinterpret_cast<const char *>(data), static_cast<int>(len)));
    if (!impl->doc || impl->doc->is_locked()) {
        return nullptr;
    }
    return std::make_unique<PopplerDocument>(std::move(impl));
}

} // namespace senpai_poppler
