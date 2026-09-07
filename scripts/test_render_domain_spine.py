#!/usr/bin/env python3
"""Exercise declaration rendering with a fixture in rustdoc's JSON shape."""

import copy
import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from render_domain_spine import HEADER, MAX_LINES, Renderer, build_json, format_rust, main, write_files


EMPTY_GENERICS = {'params': [], 'where_predicates': []}


def path(name, identity, args=None):
    return {'path': name, 'id': identity, 'args': args}


def function(inputs=(), output=None, *, generics=None, has_body=True):
    return {
        'sig': {'inputs': list(inputs), 'output': output, 'is_c_variadic': False},
        'generics': generics or EMPTY_GENERICS,
        'header': {'is_const': False, 'is_async': False, 'is_unsafe': False, 'abi': 'Rust'},
        'has_body': has_body,
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
         attrs=['automatically_derived'])
    item(10, 'Event', 'enum', {'generics': EMPTY_GENERICS, 'variants': [11, 12, 14], 'impls': []}, line=10)
    item(11, 'Idle', 'variant', {'kind': 'plain', 'discriminant': None}, line=11)
    item(12, 'Value', 'variant', {'kind': {'tuple': [13]}, 'discriminant': None}, line=12)
    item(13, '0', 'struct_field', {'primitive': 'u32'}, line=12)
    item(14, 'Named', 'variant', {'kind': {'struct': {'fields': [2], 'has_stripped_fields': False}}, 'discriminant': None}, line=14)
    item(15, 'Read', 'trait', {'generics': EMPTY_GENERICS, 'bounds': [], 'is_auto': False,
        'is_unsafe': False, 'items': [16, 17], 'implementations': [6]}, line=15)
    item(16, 'Output', 'assoc_type', {'generics': EMPTY_GENERICS, 'bounds': [], 'type': None}, line=16)
    item(17, 'read', 'function', function(output={'qualified_path': {
        'name': 'Output', 'args': None, 'self_type': {'generic': 'Self'}, 'trait': path('Read', 15)}}, has_body=False), line=17)
    item(20, 'fetch', 'function', function(output=record_type), line=20)
    item(30, 'Alias', 'use', {'source': 'sample::example::Record', 'name': 'Alias', 'id': 1, 'is_glob': False}, line=30)
    item(40, 'Token', 'struct', {'kind': 'unit', 'generics': EMPTY_GENERICS, 'impls': [41, 42, 43]}, line=40)
    token_impl = {**impl, 'for': {'resolved_path': path('Token', 40)}, 'items': []}
    item(41, None, 'impl', {**token_impl, 'trait': path('Clone', 101)}, line=40,
         attrs=['automatically_derived'])
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
    def test_dependency_paths_use_public_reexports_across_crate_local_ids(self):
        dependency = fixture()
        dependency['index']['0']['inner']['module']['items'] = [30]
        dependency['index']['30']['inner']['use']['name'] = 'PublicRecord'
        exports = Renderer(dependency).exports()
        consumer = fixture()
        consumer['index']['0']['name'] = 'consumer'
        consumer['paths']['200'] = {'crate_id': 7, 'path': ['sample', 'example', 'Record']}
        args = {'angle_bracketed': {'args': [{'type': {'primitive': 'u32'}}], 'constraints': []}}
        renderer = Renderer(consumer, exports)
        self.assertEqual(renderer.path(path('Record', 200, args)), 'sample::PublicRecord<u32>')

    def test_dependency_public_module_reexport_replaces_private_definition_path(self):
        dependency = fixture()
        dependency['index']['50'] = {
            **dependency['index']['0'], 'id': 50, 'name': 'public',
            'inner': {'module': {'items': [30], 'is_crate': False}},
        }
        dependency['index']['0']['inner']['module']['items'] = [50]
        consumer = fixture()
        consumer['index']['0']['name'] = 'consumer'
        consumer['paths']['200'] = {'crate_id': 7, 'path': ['sample', 'example', 'Record']}
        renderer = Renderer(consumer, Renderer(dependency).exports())
        self.assertEqual(renderer.path(path('Record', 200)), 'sample::public::Alias')

    def test_cfg_trace_preserves_nested_conditions_on_a_method(self):
        document = fixture()
        document['index']['3']['attrs'] = [{'other':
            '#[attr = CfgTrace([All([NameValue { name: "feature", value: Some("fixture"), '
            'span: src/lib.rs:2:11: 2:30 (#0) }, Not(NameValue { name: "unix", value: None, '
            'span: src/lib.rs:2:36: 2:40 (#0) }, src/lib.rs:2:35: 2:41 (#0))], '
            'src/lib.rs:2:10: 2:42 (#0))])]'}]
        block = Renderer(document).block(document['index']['1'])
        self.assertIn('#[cfg(all(feature = "fixture", not(unix)))]\n    pub fn new', block)

    def test_gated_reexport_propagates_its_condition_to_the_type_and_impls(self):
        document = fixture()
        document['index']['0']['inner']['module']['items'] = [30]
        document['index']['30']['attrs'] = [{'other':
            '#[attr = CfgTrace([NameValue { name: "feature", value: Some("fixture"), '
            'span: src/lib.rs:2:7: 2:26 (#0) }])]'}]
        files = Renderer(document).files('sample')
        self.assertIn('#[cfg(feature = "fixture")]\npub struct Record<T>', files['example.md'])
        self.assertIn('#[cfg(feature = "fixture")]\nimpl Alias', files['example.md'])
        self.assertIn('#[cfg(feature = "fixture")]\npub use example::Record as Alias;', files['example.md'])

    def test_module_cfg_propagates_to_nested_declarations(self):
        document = fixture()
        document['index']['0']['attrs'] = [{'other':
            '#[attr = CfgTrace([NameValue { name: "feature", value: Some("fixture"), '
            'span: src/lib.rs:2:7: 2:26 (#0) }])]'}]
        block = Renderer(document).block(document['index']['20'])
        self.assertIn('#[cfg(feature = "fixture")]\npub fn fetch()', block)

    def test_complementary_source_exports_keep_the_public_type_available(self):
        document = fixture()
        document['index']['0']['inner']['module']['items'] = [30]
        export = document['index']['30']
        export['span']['filename'] = 'src/lib.rs'
        export['inner']['use']['name'] = 'Record'
        export['attrs'] = [{'other':
            '#[attr = CfgTrace([NameValue { name: "target_os", value: Some("linux"), '
            'span: src/lib.rs:2:7: 2:26 (#0) }])]'}]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / 'src').mkdir()
            for counterpart, unconditional in [
                ('unsupported::{Other, Record}', True),
                ('unsupported::Fallback as Record', True),
                ('unsupported::Other', False),
            ]:
                with self.subTest(counterpart=counterpart):
                    (root / 'src/lib.rs').write_text(
                        '#[cfg(target_os = "linux")]\npub use sandbox::Record;\n'
                        '#[cfg(not(target_os = "linux"))]\npub use ' + counterpart + ';\n')
                    with patch('render_domain_spine.ROOT', root):
                        block = Renderer(document).block(document['index']['1'])
                    self.assertEqual('#[cfg(target_os = "linux")]' not in block, unconditional)
                    self.assertIn('pub struct Record', block)
                    self.assertIn('pub fn new', block)

    def test_unconditional_export_does_not_inherit_a_gated_alias_condition(self):
        document = fixture()
        document['index']['30']['attrs'] = [{'other':
            '#[attr = CfgTrace([NameValue { name: "feature", value: Some("fixture"), '
            'span: src/lib.rs:2:7: 2:26 (#0) }])]'}]
        block = Renderer(document).block(document['index']['1'])
        self.assertNotIn('#[cfg(', block)

    def test_alternative_gated_exports_keep_either_feature_available(self):
        document = fixture()
        document['index']['0']['inner']['module']['items'] = [30, 31]
        document['index']['30']['attrs'] = [{'other':
            '#[attr = CfgTrace([NameValue { name: "feature", value: Some("first"), '
            'span: src/lib.rs:2:7: 2:26 (#0) }])]'}]
        document['index']['31'] = {
            **copy.deepcopy(document['index']['30']), 'id': 31,
            'attrs': [{'other': '#[attr = CfgTrace([NameValue { name: "feature", '
                       'value: Some("second"), span: src/lib.rs:3:7: 3:27 (#0) }])]'}],
        }
        document['index']['31']['inner']['use']['name'] = 'OtherAlias'
        block = Renderer(document).block(document['index']['1'])
        self.assertIn('#[cfg(any(all(feature = "first"), all(feature = "second")))]', block)

    def test_external_blanket_impl_with_unbound_type_parameter_is_excluded(self):
        document = fixture()
        implementation = document['index']['6']['inner']['impl']
        implementation.update(trait=path('DynClone', 104), blanket_impl={'generic': 'T'})
        implementation['generics'] = {'params': [{'name': 'T', 'kind': {'type': {
            'bounds': [], 'default': None, 'is_synthetic': False}}}], 'where_predicates': []}
        document['paths']['104'] = {'crate_id': 1, 'path': ['dyn_clone', 'DynClone']}
        block = Renderer(document).block(document['index']['1'])
        self.assertNotIn('DynClone', block)
        self.assertIn('impl Record', block)

    def test_substituted_external_blanket_impl_is_excluded_with_generic_trait_arguments(self):
        document = fixture()
        implementation = document['index']['6']['inner']['impl']
        implementation.update(trait=path('FromRef', 104, {'angle_bracketed': {
            'args': [{'type': {'generic': 'T'}}], 'constraints': []}}), blanket_impl={'generic': 'T'})
        implementation['generics'] = {'params': [{'name': 'T', 'kind': {'type': {
            'bounds': [], 'default': None, 'is_synthetic': False}}}], 'where_predicates': []}
        document['paths']['104'] = {'crate_id': 1, 'path': ['axum', 'extract', 'FromRef']}
        block = Renderer(document).block(document['index']['1'])
        self.assertNotIn('FromRef', block)
        self.assertIn('impl Record', block)

    def test_external_blanket_impl_keeps_its_unsubstituted_self_type(self):
        document = fixture()
        implementation = document['index']['6']['inner']['impl']
        implementation.update(trait=path('DynClone', 104), blanket_impl={'generic': 'T'})
        implementation['for']['resolved_path']['args'] = {'angle_bracketed': {
            'args': [{'type': {'generic': 'T'}}], 'constraints': []}}
        implementation['blanket_impl'] = copy.deepcopy(implementation['for'])
        implementation['generics'] = {'params': [{'name': 'T', 'kind': {'type': {
            'bounds': [], 'default': None, 'is_synthetic': False}}}], 'where_predicates': []}
        document['paths']['104'] = {'crate_id': 1, 'path': ['dyn_clone', 'DynClone']}
        block = Renderer(document).block(document['index']['1'])
        self.assertIn('impl<T> dyn_clone::DynClone for Record<T>', block)

    def test_primitive_representation_does_not_add_a_conflicting_rust_hint(self):
        document = fixture()
        document['index']['10']['attrs'] = [
            {'repr': {'kind': 'rust', 'align': None, 'packed': None, 'int': 'u8'}},
        ]
        renderer = Renderer(document)
        block = renderer.block(renderer.item(10))
        self.assertIn('#[repr(u8)]\npub enum Event', block)
        self.assertNotIn('repr(Rust,', block)

    def test_provided_trait_methods_differ_from_required_methods(self):
        document = fixture()
        provided = copy.deepcopy(document['index']['17'])
        provided.update(id=18, name='read_default')
        provided['inner']['function']['has_body'] = True
        document['index']['18'] = provided
        document['index']['15']['inner']['trait']['items'].append(18)
        renderer = Renderer(document)
        block = renderer.block(renderer.item(15))
        self.assertIn('    fn read() -> <Self as Read>::Output;', block)
        self.assertIn('    fn read_default() -> <Self as Read>::Output {\n        /* provided */\n    }', block)
        self.assertNotIn('provided', renderer.impl_block(renderer.item(6)))
        self.assertIn('fn read(&self) -> T;', renderer.impl_block(renderer.item(6)))
        self.assertEqual(renderer.declaration(renderer.item(20)), 'pub fn fetch() -> Record;')

    def test_caller_attributes_stay_above_types_variants_and_methods(self):
        document = fixture()
        document['index']['1']['attrs'] = [
            {'must_use': {'reason': None}},
            {'repr': {'kind': 'c', 'align': 8, 'packed': None, 'int': None}},
            {'other': '#[allow(dead_code)]'},
            {'other': '#[attr = Inline(Hint)]'},
        ]
        document['index']['3']['attrs'] = [{'must_use': {'reason': 'keep "value"\\bytes\n\x00'}}]
        document['index']['2']['deprecation'] = {'since': None, 'note': 'use read'}
        document['index']['10']['attrs'] = ['non_exhaustive']
        document['index']['11']['attrs'] = ['non_exhaustive']
        document['index']['17']['deprecation'] = {'since': '1.0', 'note': 'use fetch'}
        renderer = Renderer(document)
        record = renderer.block(renderer.item(1))
        self.assertIn('#[must_use]\n#[repr(C, align(8))]\npub struct Record', record)
        self.assertIn('    #[deprecated(note = "use read")]\n    pub value: T,', record)
        self.assertIn('    #[must_use = "keep \\"value\\"\\\\bytes\\n\\u{0}"]\n    pub fn new', record)
        self.assertNotIn('allow(dead_code)', record)
        self.assertNotIn('Inline(Hint)', record)
        self.assertIn('#[non_exhaustive]\npub enum Event', renderer.block(renderer.item(10)))
        self.assertIn('    #[non_exhaustive]\n    Idle,', renderer.block(renderer.item(10)))
        self.assertIn('    #[deprecated(since = "1.0", note = "use fetch")]\n    fn read',
                      renderer.block(renderer.item(15)))

    def test_handwritten_blanket_impl_keeps_generics_bounds_and_associated_items(self):
        document = fixture()
        implementation = document['index']['6']['inner']['impl']
        implementation['blanket_impl'] = {'generic': 'T'}
        implementation['for'] = {'generic': 'T'}
        implementation['generics'] = {
            'params': [{'name': 'T', 'kind': {'type': {
                'bounds': [], 'default': None, 'is_synthetic': False}}}],
            'where_predicates': [{'bound_predicate': {
                'type': {'generic': 'T'}, 'generic_params': [], 'bounds': [{'trait_bound': {
                    'trait': path('Debug', 100), 'generic_params': [], 'modifier': 'none'}}]}}],
        }
        renderer = Renderer(document)
        block = renderer.block(renderer.item(15))
        self.assertIn('impl<T> Read for T\nwhere\n    T: fmt::Debug,\n{', block)
        self.assertIn('    fn read(&self) -> T;\n    type Output = T;', block)
        self.assertNotIn('// derives: Read', block)
        self.assertIn('// derives: fmt::Debug', renderer.block(renderer.item(1)))
        self.assertNotIn('Borrow', renderer.block(renderer.item(40)))

    def test_struct_fields_and_impls_are_bare_declarations(self):
        renderer = Renderer(fixture())
        self.assertEqual(renderer.block(renderer.item(1)), '''## Record

```rust
pub struct Record<T> {
    pub value: T,
    /* private */
}
// derives: fmt::Debug
impl Record {
    pub fn new(value: T) -> Self;
}
impl Read for Record {
    fn read(&self) -> T;
    type Output = T;
}
```''')

    def test_inherent_constants_keep_visibility_and_trait_constants_omit_it(self):
        document = fixture()
        constant = copy.deepcopy(document['index']['7'])
        constant.update(name='LIMIT', inner={'assoc_const': {'type': {'primitive': 'usize'}}})
        document['index']['7'] = constant
        document['index']['4']['inner']['impl']['items'].append(7)
        document['index']['15']['inner']['trait']['items'].append(7)
        renderer = Renderer(document)
        self.assertIn('    pub const LIMIT: usize;', renderer.impl_block(renderer.item(4)))
        self.assertIn('    const LIMIT: usize;', renderer.impl_block(renderer.item(6)))
        self.assertIn('    const LIMIT: usize;', renderer.declaration(renderer.item(15)))

    def test_local_paths_use_public_root_reexports_and_preserve_arguments(self):
        document = fixture()
        root = document['index']['0']['inner']['module']['items']
        root.remove(1)
        document['index']['30']['inner']['use']['name'] = 'Record'
        renderer = Renderer(document)
        args = {'angle_bracketed': {'args': [{'type': {'primitive': 'u32'}}], 'constraints': []}}
        self.assertEqual(renderer.path(path('sample::example::Record', 1, args)), 'Record<u32>')
        self.assertEqual(renderer.path(path('core::fmt::Debug', 100)), 'fmt::Debug')
        document['index']['30']['inner']['use']['name'] = 'PublicRecord'
        self.assertEqual(Renderer(document).path(path('Record', 1)), 'PublicRecord')

    def test_json_build_uses_configured_toolchain_for_baseline_workspace(self):
        with patch('render_domain_spine.configuration', return_value={'rustdoc_toolchain': 'test-toolchain'}), \
                patch('render_domain_spine.subprocess.run') as run:
            result = build_json('sample-crate', workspace=Path('/tmp/baseline'), target=Path('/tmp/output'))
        command = run.call_args.args[0]
        self.assertEqual(command[:3], ['cargo', '+test-toolchain', 'rustdoc'])
        self.assertEqual(command[command.index('--manifest-path') + 1], '/tmp/baseline/Cargo.toml')
        self.assertEqual(result, Path('/tmp/output/doc/sample_crate.json'))

    def test_conversion_impl_stays_with_its_subject_or_local_argument(self):
        document = fixture()
        implementation = copy.deepcopy(document['index']['4'])
        implementation['id'] = 50
        implementation['inner']['impl'].update(
            trait=path('From', 104, {'angle_bracketed': {
                'args': [{'type': {'resolved_path': path('Record', 1)}}], 'constraints': []}}),
            **{'for': {'resolved_path': path('Token', 40)}, 'items': []},
        )
        document['index']['50'] = implementation
        document['paths']['104'] = {'crate_id': 1, 'path': ['core', 'convert', 'From']}
        document['index']['1']['inner']['struct']['impls'].append(50)
        document['index']['40']['inner']['struct']['impls'].append(50)
        renderer = Renderer(document)
        declaration = 'impl convert::From<Record> for Token {}'
        self.assertNotIn(declaration, renderer.block(renderer.item(1)))
        self.assertEqual(renderer.block(renderer.item(40)).count(declaration), 1)
        document['paths']['105'] = {'crate_id': 1, 'path': ['alloc', 'string', 'String']}
        implementation['inner']['impl']['for'] = {'resolved_path': path('String', 105)}
        document['index']['40']['inner']['struct']['impls'].remove(50)
        renderer = Renderer(document)
        self.assertIn('impl convert::From<Record> for string::String {}', renderer.block(renderer.item(1)))

    def test_c_abi_retains_its_required_capitalization(self):
        renderer = Renderer(fixture())
        for unwind, abi in [(False, 'C'), (True, 'C-unwind')]:
            with self.subTest(abi=abi):
                item = copy.deepcopy(renderer.item(20))
                item['inner']['function']['header']['abi'] = {'C': {'unwind': unwind}}
                self.assertEqual(renderer.declaration(item), f'pub extern "{abi}" fn fetch() -> Record;')

    def test_rustfmt_wraps_long_function_signatures(self):
        code = (
            'pub fn very_long_method_name_with_long_parameters('
            'first_parameter: first_module::FirstType, '
            'second_parameter: second_module::SecondType, '
            'third_parameter: third_module::ThirdType) '
            '-> result::Result<response_module::ResponseType, failure_module::FailureType>;'
        )
        self.assertEqual(format_rust(code), '''pub fn very_long_method_name_with_long_parameters(
    first_parameter: first_module::FirstType,
    second_parameter: second_module::SecondType,
    third_parameter: third_module::ThirdType,
) -> result::Result<response_module::ResponseType, failure_module::FailureType>;''')

    def test_long_attribute_messages_survive_formatting_and_fallback(self):
        reason = ('Discarding this value loses the handle needed to observe completion and '
                  'report the operation result to the caller that requested the work.')
        code = '#[must_use = "' + reason + '"]\npub fn handle() -> Handle;'
        self.assertIn('"' + reason + '"', format_rust(code))
        rejected = subprocess.CompletedProcess([], 1, stdout='', stderr='rejected declaration')
        with patch('render_domain_spine.subprocess.run', return_value=rejected):
            self.assertIn('"' + reason + '"', format_rust(code))

    def test_rejected_synthesized_block_keeps_hand_layout(self):
        renderer = Renderer(fixture())
        rejected = subprocess.CompletedProcess([], 1, stdout='', stderr='rejected declaration')
        with patch('render_domain_spine.subprocess.run', return_value=rejected):
            block = renderer.block(renderer.item(1))
        self.assertIn('pub struct Record<T> {\n    pub value: T,\n    /* private */\n}', block)
        self.assertIn('    pub fn new(value: T) -> Self;', block)
        self.assertNotIn('rejected declaration', block)

    def test_rejected_long_signature_falls_back_to_wrapped_declaration(self):
        code = (
            'pub fn synthesized(first_parameter: first_module::FirstType, '
            'second_parameter: second_module::SecondType, '
            'third_parameter: third_module::ThirdType);'
        )
        rejected = subprocess.CompletedProcess([], 1, stdout='', stderr='rejected declaration')
        with patch('render_domain_spine.subprocess.run', return_value=rejected):
            formatted = format_rust(code)
        self.assertEqual(' '.join(formatted.split()), code)
        self.assertGreater(len(formatted.splitlines()), 1)
        self.assertTrue(all(len(line) <= 100 for line in formatted.splitlines()))

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
    fn read() -> <Self as Read>::Output;
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

    def test_nested_source_span_stays_in_its_top_level_module_page(self):
        document = fixture()
        document['index']['1']['span']['filename'] = 'src/example/inner.rs'
        renderer = Renderer(document)
        modules = renderer.modules()
        self.assertEqual(list(modules), ['example'])
        self.assertIn(renderer.item(1), modules['example'])
        files = renderer.files('sample')
        self.assertEqual(set(files), {'example.md', 'README.md'})
        self.assertIn('pub struct Record<T>', files['example.md'])

    def test_external_reexport_uses_the_export_name_when_item_name_is_null(self):
        document = fixture()
        export = document['index']['30']
        export['name'] = None
        export['inner']['use'].update(source='external::Value', name='Alias', id=100)
        files = Renderer(document).files('sample')
        self.assertIn('## Alias\n\n```rust\npub use external::Value as Alias;\n```',
                      files['example.md'])

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
            "pub fn fetch<'a, T, const N: usize = 4>() -> Record where T: fmt::Debug;")

    def test_borrowed_trait_object_groups_its_lifetime_bound(self):
        renderer = Renderer(fixture())
        reference = {'borrowed_ref': {
            'lifetime': None, 'is_mutable': False,
            'type': {'dyn_trait': {
                'traits': [{'trait': path('Debug', 100), 'generic_params': []}],
                'lifetime': "'static",
            }},
        }}
        self.assertEqual(renderer.type(reference), "&(dyn fmt::Debug + 'static)")

    def test_reference_to_impl_trait_groups_multiple_bounds(self):
        renderer = Renderer(fixture())
        reference = {'borrowed_ref': {
            'lifetime': None, 'is_mutable': True,
            'type': {'impl_trait': [
                {'trait_bound': {'trait': path('Debug', 100), 'modifier': 'none', 'generic_params': []}},
                {'trait_bound': {'trait': path('Send', 102), 'modifier': 'none', 'generic_params': []}},
            ]},
        }}
        self.assertEqual(renderer.type(reference), '&mut (impl fmt::Debug + marker::Send)')

    def test_module_at_line_limit_splits_by_kind_and_links_every_part(self):
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

    def test_existing_split_pages_keep_item_boundaries_when_declarations_shrink(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            directory = root / 'docs/api/sample/example'
            directory.mkdir(parents=True)
            (directory / 'types.md').write_text('# example: types\n\n## Record\n')
            (directory / 'types-2.md').write_text('# example: types-2\n\n## Token\n')
            with patch('render_domain_spine.ROOT', root):
                files = Renderer(fixture()).files('sample')
        self.assertNotIn('example.md', files)
        self.assertIn('## Record', files['example/types.md'])
        self.assertNotIn('## Event', files['example/types.md'])
        self.assertIn('## Event', files['example/types-2.md'])
        self.assertIn('## Token', files['example/types-2.md'])

    def test_split_page_survives_removal_of_preceding_pages_trailing_declaration(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            directory = root / 'docs/api/sample/example'
            directory.mkdir(parents=True)
            (directory / 'types.md').write_text('# example: types\n\n## Record\n\n## Removed\n')
            (directory / 'types-2.md').write_text('# example: types-2\n\n## Event\n\n## Token\n')
            with patch('render_domain_spine.ROOT', root):
                files = Renderer(fixture()).files('sample')
        self.assertIn('## Record', files['example/types.md'])
        self.assertNotIn('## Event', files['example/types.md'])
        self.assertIn('## Event', files['example/types-2.md'])
        self.assertIn('## Token', files['example/types-2.md'])
        self.assertNotIn('example/types-3.md', files)

    def test_later_split_page_survives_an_empty_first_page(self):
        document = fixture()
        document['index']['0']['inner']['module']['items'] = [10, 40]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            directory = root / 'docs/api/sample/example'
            directory.mkdir(parents=True)
            (directory / 'types.md').write_text('# example: types\n\n## Record\n')
            (directory / 'types-2.md').write_text('# example: types-2\n\n## Event\n\n## Token\n')
            with patch('render_domain_spine.ROOT', root):
                files = Renderer(document).files('sample')
                for name, content in files.items():
                    (root / 'docs/api/sample' / name).write_text(content)
                self.assertEqual(Renderer(document).files('sample'), files)
        self.assertNotIn('## ', files['example/types.md'])
        self.assertIn('## Event', files['example/types-2.md'])
        self.assertIn('## Token', files['example/types-2.md'])
        self.assertIn('](example/types-2.md)', files['README.md'])

    def test_empty_module_render_is_stable_after_removing_all_split_declarations(self):
        document = fixture()
        document['index']['50'] = {
            **copy.deepcopy(document['index']['0']), 'id': 50, 'name': 'example',
            'inner': {'module': {'items': [], 'is_crate': False}},
        }
        document['index']['0']['inner']['module']['items'] = [50]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            directory = root / 'docs/api/sample'
            with patch('render_domain_spine.ROOT', root):
                write_files(directory, {'example/types.md': '## Record\n',
                                        'example/types-2.md': '## Event\n'})
                files = Renderer(document).files('sample')
                write_files(directory, files)
                self.assertEqual(Renderer(document).files('sample'), files)
                (directory / 'example').rmdir()
                self.assertEqual(Renderer(document).files('sample'), files)
        self.assertEqual(set(files), {'README.md', 'example.md'})
        self.assertNotIn('## ', files['example.md'])
        self.assertIn('| example | 0 | 0 | 0 | [example](example.md) |', files['README.md'])

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

    def test_prebuilt_json_renders_to_separate_output_and_preserves_page_boundaries(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / 'source'
            page_directory = source / 'docs/api/sample/example'
            page_directory.mkdir(parents=True)
            boundary = page_directory / 'types.md'
            previous = '# example: types\n\n## Record\n'
            boundary.write_text(previous)
            (source / 'docs/api/crates.toml').write_text('crates = ["sample", "second"]\n')
            json_directories = [root / 'first-json', root / 'second-json']
            for directory, crate in zip(json_directories, ['sample', 'second']):
                directory.mkdir()
                (directory / f'{crate}.json').write_text(json.dumps(fixture()))
            output = root / 'output'
            arguments = ['render', '--source-root', str(source), '--output-dir', str(output),
                         '--json-dir', *map(str, json_directories)]
            with patch('sys.argv', arguments), patch('render_domain_spine.build_json') as build:
                main()
            build.assert_not_called()
            self.assertEqual(boundary.read_text(), previous)
            self.assertIn('## Record', (output / 'sample/example/types.md').read_text())
            self.assertTrue((output / 'second/README.md').is_file())

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
