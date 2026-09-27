//! Lookups run on every frame, so they must not allocate. This test binary counts the
//! allocations made on the current thread through its own global allocator.

use i18n::Catalog as _;
use i18n_catalog::{Layer, PoCatalog};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct CountingAllocator;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.with(|count| count.set(count.get() + 1));
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn allocations_during(work: impl FnOnce()) -> usize {
    let before = ALLOCATIONS.with(Cell::get);
    work();
    ALLOCATIONS.with(Cell::get) - before
}

#[test]
fn lookups_do_not_allocate() {
    let catalog = PoCatalog::from_layers(
        "ru",
        vec![Layer {
            name: "test".to_string(),
            files: vec![(
                "test.po".to_string(),
                concat!(
                    "msgid \"\"\nmsgstr \"Plural-Forms: nplurals=3; plural=(n%10==1 && n%100!=11 ? 0 : n%10>=2 && n%10<=4 && (n%100<10 || n%100>=20) ? 1 : 2);\\n\"\n\n",
                    "msgid \"Open\"\nmsgstr \"Открыть\"\n\n",
                    "msgctxt \"menu\"\nmsgid \"Open\"\nmsgstr \"Открыть…\"\n\n",
                    "msgctxt \"menu\"\nmsgid \"{count} file\"\nmsgid_plural \"{count} files\"\n",
                    "msgstr[0] \"{count} файл\"\nmsgstr[1] \"{count} файла\"\nmsgstr[2] \"{count} файлов\"\n",
                )
                .to_string(),
            )],
        }],
        Vec::new(),
    );
    assert!(catalog.problems().is_empty());

    let mut results = [None; 7];
    let allocations = allocations_during(|| {
        results = [
            catalog.lookup(None, "Open"),
            catalog.lookup(Some("menu"), "Open"),
            catalog.lookup(Some("menu"), "Missing"),
            catalog.lookup(Some("missing context"), "Open"),
            catalog.lookup(None, "Missing"),
            catalog.lookup_plural(Some("menu"), "{count} file", "{count} files", 5),
            catalog.lookup_plural(None, "{count} file", "{count} files", 5),
        ];
    });
    assert_eq!(
        results,
        [
            Some("Открыть"),
            Some("Открыть…"),
            None,
            None,
            None,
            Some("{count} файлов"),
            None,
        ]
    );
    assert_eq!(allocations, 0, "lookups allocated");
}
