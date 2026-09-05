#!/usr/bin/env python3
"""Exercise declaration rendering with a fixture in rustdoc's JSON shape."""

import copy
import json
import tempfile
import unittest
from pathlib import Path

from render_domain_spine import HEADER, MAX_LINES, Renderer, write_files


EMPTY_GENERICS = {'params': [], 'where_predicates': []}


def path(name, identity, args=None):
    return {'path': name, 'id': identity, 'args': args}


def function(inputs=(), output=None, *, generics=None):
    return {
        'sig': {'inputs': list(inputs), 'output': output, 'is_c_variadic': False},
        'generics': generics or EMPTY_GENERICS,
        'header': {'is_const': False, 'is_async': False, 'is_unsafe': False, 'abi': 'Rust'},
        'has_body': True,
    }


def fixture():
    """A public generic record, data enum, trait, re-export and derived unit."""
    index = {}

    def item(identity, name, kind, body, *, line, visibility='public', attrs=()):
        index[str(identity)] = {
            'id': identity, 'crate_id': 0, 'name': name, 'visibility': visibility,
            'span': {'filename': 'src/example.rs', 'begin': [line, 1], 'end': [line, 2]},
            'attrs': list(attrs), 'docs': 'Documentation must not enter declarations.',
            'inner': {kind: body},
        }

    record_type = {'resolved_path': path('Record', 1)}
    generic_t = {'generic': 'T'}
    item(0, 'sample', 'module', {'items': [20, 1, 10, 15, 30, 40], 'is_crate': True}, line=1)
    item(1, 'Record', 'struct', {
        'kind': {'plain': {'fields': [2], 'has_stripped_fields': True}},
        'generics': {'params': [{'name': 'T', 'kind': {'type': {
            'bounds': [], 'default': None, 'is_synthetic': False}}}], 'where_predicates': []},
        'impls': [4, 6, 8],
    }, line=2)
    item(2, 'value', 'struct_field', generic_t, line=3)
    item(3, 'new', 'function', function([['value', generic_t]], {'generic': 'Self'}), line=4)
    impl = {
        'is_unsafe': False, 'generics': EMPTY_GENERICS, 'provided_trait_methods': [],
        'trait': None, 'for': record_type, 'items': [3],
        'is_negative': False, 'is_synthetic': False, 'blanket_impl': None,
    }
    item(4, None, 'impl', impl, line=4, visibility='default')
    item(5, 'read', 'function', function([['self', {'borrowed_ref': {
        'lifetime': None, 'is_mutable': False, 'type': {'generic': 'Self'}}}]], generic_t), line=6)
    item(6, None, 'impl', {**impl, 'trait': path('Read', 15), 'items': [5, 7]}, line=6)
    item(7, 'Output', 'assoc_type', {'generics': EMPTY_GENERICS, 'bounds': [], 'type': generic_t}, line=7)
    item(8, None, 'impl', {**impl, 'trait': path('Debug', 100), 'items': []}, line=2,
         attrs=[{'other': '#[automatically_derived]'}])
    item(10, 'Event', 'enum', {'generics': EMPTY_GENERICS, 'variants': [11, 12, 14], 'impls': []}, line=10)
    item(11, 'Idle', 'variant', {'kind': 'plain', 'discriminant': None}, line=11)
    item(12, 'Value', 'variant', {'kind': {'tuple': [13]}, 'discriminant': None}, line=12)
    item(13, '0', 'struct_field', {'primitive': 'u32'}, line=12)
    item(14, 'Named', 'variant', {'kind': {'struct': {'fields': [2], 'has_stripped_fields': False}}, 'discriminant': None}, line=14)
    item(15, 'Read', 'trait', {'generics': EMPTY_GENERICS, 'bounds': [], 'is_auto': False,
        'is_unsafe': False, 'items': [16, 17], 'implementations': [6]}, line=15)
    item(16, 'Output', 'assoc_type', {'generics': EMPTY_GENERICS, 'bounds': [], 'type': None}, line=16)
    item(17, 'read', 'function', function(output={'qualified_path': {
        'name': 'Output', 'args': None, 'self_type': {'generic': 'Self'}, 'trait': path('Read', 15)}}), line=17)
    item(20, 'fetch', 'function', function(output=record_type), line=20)
    item(30, 'Alias', 'use', {'source': 'sample::example::Record', 'name': 'Alias', 'id': 1, 'is_glob': False}, line=30)
    item(40, 'Token', 'struct', {'kind': 'unit', 'generics': EMPTY_GENERICS, 'impls': [41, 42, 43]}, line=40)
    token_impl = {**impl, 'for': {'resolved_path': path('Token', 40)}, 'items': []}
    item(41, None, 'impl', {**token_impl, 'trait': path('Clone', 101)}, line=40,
         attrs=[{'other': '#[automatically_derived]'}])
    item(42, None, 'impl', {**token_impl, 'trait': path('Send', 102), 'is_synthetic': True}, line=40)
    item(43, None, 'impl', {**token_impl, 'trait': path('Borrow', 103), 'blanket_impl': {'generic': 'T'}}, line=40)
    paths = {
        str(identity): {'crate_id': 0, 'path': ['sample', 'example', index[str(identity)]['name']]}
        for identity in (1, 10, 15, 40)
    }
    paths.update({
        '100': {'crate_id': 1, 'path': ['core', 'fmt', 'Debug']},
        '101': {'crate_id': 1, 'path': ['core', 'clone', 'Clone']},
        '102': {'crate_id': 1, 'path': ['core', 'marker', 'Send']},
        '103': {'crate_id': 1, 'path': ['core', 'borrow', 'Borrow']},
    })
    # Round-trip through JSON so index keys and payload types match rustdoc output.
    return json.loads(json.dumps({'root': 0, 'index': index, 'paths': paths}))


class RenderDomainSpineTests(unittest.TestCase):
    def test_struct_fields_and_impls_are_bare_declarations(self):
        renderer = Renderer(fixture())
        self.assertEqual(renderer.block(renderer.item(1)), '''## Record

```rust
pub struct Record<T> { pub value: T, /* private */ }
// derives: fmt::Debug
impl example::Record {
    pub fn new(value: T) -> Self;
}
impl example::Read for example::Record {
    pub fn read(&self) -> T;
    type Output = T;
}
```''')

    def test_enum_keeps_data_variant_shapes(self):
        renderer = Renderer(fixture())
        self.assertEqual(renderer.declaration(renderer.item(10)), '''pub enum Event {
    Idle,
    Value(u32),
    Named { value: T },
}''')

    def test_trait_keeps_associated_type_and_method(self):
        renderer = Renderer(fixture())
        self.assertEqual(renderer.declaration(renderer.item(15)), '''pub trait Read {
    type Output;
    pub fn read() -> <Self as example::Read>::Output;
}''')

    def test_derived_only_type_has_one_derive_line_and_no_blanket_noise(self):
        renderer = Renderer(fixture())
        self.assertEqual(renderer.block(renderer.item(40)), '''## Token

```rust
pub struct Token;
// derives: clone::Clone
```''')

    def test_reexports_do_not_duplicate_types_and_items_follow_source_order(self):
        files = Renderer(fixture()).files('sample')
        text = files['example.md']
        self.assertEqual(text.count('pub struct Record<T>'), 1)
        self.assertIn('pub use example::Record as Alias;', text)
        self.assertLess(text.index('## Record'), text.index('## fetch'))
        self.assertIn('| example | 3 | 1 | 1 | [example](example.md) |', files['README.md'])
        self.assertNotIn('Documentation must not', text)

    def test_generic_constraints_and_external_paths_are_preserved(self):
        document = fixture()
        renderer = Renderer(document)
        generics = {'params': [
            {'name': "'a", 'kind': {'lifetime': {'outlives': []}}},
            {'name': 'T', 'kind': {'type': {'bounds': [], 'default': None, 'is_synthetic': False}}},
            {'name': 'N', 'kind': {'const': {'type': {'primitive': 'usize'}, 'default': '4'}}},
        ], 'where_predicates': [{'bound_predicate': {'type': {'generic': 'T'},
            'generic_params': [], 'bounds': [{'trait_bound': {
                'trait': path('Debug', 100), 'modifier': 'none', 'generic_params': []}}]}}]}
        value = copy.deepcopy(renderer.item(20))
        value['inner']['function']['generics'] = generics
        self.assertEqual(renderer.declaration(value),
            "pub fn fetch<'a, T, const N: usize = 4>() -> example::Record where T: fmt::Debug;")

    def test_module_at_800_lines_splits_by_kind_and_links_every_part(self):
        document = fixture()
        renderer = Renderer(document)
        original_lines = len(renderer.files('sample')['example.md'].splitlines())
        extra_variants = MAX_LINES - original_lines
        enum = document['index']['10']['inner']['enum']
        for offset in range(extra_variants):
            identity = 1000 + offset
            variant = copy.deepcopy(document['index']['11'])
            variant.update(id=identity, name=f'Extra{offset}')
            document['index'][str(identity)] = variant
            enum['variants'].append(identity)
        files = Renderer(document).files('sample')
        self.assertNotIn('example.md', files)
        self.assertIn('example/types.md', files)
        self.assertIn('example/traits.md', files)
        self.assertIn('example/functions.md', files)
        for name, content in files.items():
            self.assertLess(len(content.splitlines()), MAX_LINES, name)
            self.assertLess(len(content.encode()), 120_000, name)
            self.assertTrue(content.startswith(HEADER), name)
            if name != 'README.md':
                self.assertIn(f']({name})', files['README.md'])

    def test_oversized_type_group_continues_at_item_boundaries(self):
        document = fixture()
        root_items = document['index']['0']['inner']['module']['items']
        for offset in range(MAX_LINES // 5):
            identity = 1000 + offset
            item = copy.deepcopy(document['index']['40'])
            item.update(id=identity, name=f'Token{offset}')
            item['inner']['struct']['impls'] = []
            document['index'][str(identity)] = item
            root_items.append(identity)
        files = Renderer(document).files('sample')
        self.assertIn('example/types-2.md', files)
        self.assertIn('[types-2](example/types-2.md)', files['README.md'])
        self.assertTrue(all(len(content.splitlines()) < MAX_LINES for content in files.values()))

    def test_regeneration_removes_obsolete_pages(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            write_files(directory, {'old.md': HEADER})
            files = Renderer(fixture()).files('sample')
            write_files(directory, files)
            self.assertFalse((directory / 'old.md').exists())
            self.assertEqual((directory / 'example.md').read_text(), files['example.md'])


if __name__ == '__main__':
    unittest.main()
