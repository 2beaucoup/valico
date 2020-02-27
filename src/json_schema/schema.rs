use serde_json::Value;
use std::collections;
use std::ops;
use url::Url;

use super::helpers;
use super::keywords;
use super::scope;
use super::validators;
use std::error::Error;
use std::fmt;
use std::fmt::{Display, Formatter};

#[derive(Debug)]
pub struct WalkContext<'a> {
    pub url: &'a url::Url,
    pub fragment: Vec<String>,
    pub scopes: &'a mut collections::HashMap<String, Vec<String>>,
}

impl<'a> WalkContext<'a> {
    pub fn escaped_fragment(&self) -> String {
        helpers::connect(
            self.fragment
                .iter()
                .map(|s| s.as_ref())
                .collect::<Vec<&str>>()
                .as_ref(),
        )
    }
}

#[derive(Debug)]
#[allow(missing_copy_implementations)]
pub enum SchemaError {
    WrongId,
    IdConflicts,
    NotAnObject,
    UrlParseError(url::ParseError),
    UnknownKey(String),
    Malformed { path: String, detail: String },
}

impl Display for SchemaError {
    fn fmt(&self, f: &mut Formatter) -> Result<(), fmt::Error> {
        match *self {
            SchemaError::WrongId => write!(f, "wrong id"),
            SchemaError::IdConflicts => write!(f, "id conflicts"),
            SchemaError::NotAnObject => write!(f, "not an object"),
            SchemaError::UrlParseError(ref e) => write!(f, "url parse error: {}", e),
            SchemaError::UnknownKey(ref k) => write!(f, "unknown key: {}", k),
            SchemaError::Malformed {
                ref path,
                ref detail,
            } => write!(f, "malformed path: `{}`, details: {}", path, detail),
        }
    }
}

impl Error for SchemaError {}

#[derive(Debug)]
pub struct ScopedSchema<'a> {
    scope: &'a scope::Scope,
    schema: &'a Schema,
}

impl<'a> ops::Deref for ScopedSchema<'a> {
    type Target = Schema;

    fn deref(&self) -> &Schema {
        self.schema
    }
}

impl<'a> ScopedSchema<'a> {
    pub fn new(scope: &'a scope::Scope, schema: &'a Schema) -> ScopedSchema<'a> {
        ScopedSchema { scope, schema }
    }

    pub fn validate(&self, data: &Value) -> validators::ValidationState {
        self.schema.validate_in_scope(data, "", self.scope)
    }

    pub fn validate_in(&self, data: &Value, path: &str) -> validators::ValidationState {
        self.schema.validate_in_scope(data, path, self.scope)
    }
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct Schema {
    pub id: Option<url::Url>,
    schema: Option<url::Url>,
    original: Value,
    tree: collections::BTreeMap<String, Schema>,
    validators: validators::Validators,
    scopes: collections::HashMap<String, Vec<String>>,
    default: Option<Value>,
}

include!(concat!(env!("OUT_DIR"), "/codegen.rs"));

pub struct CompilationSettings<'a> {
    pub keywords: &'a keywords::KeywordMap,
    pub ban_unknown_keywords: bool,
    pub supply_defaults: bool,
}

impl<'a> CompilationSettings<'a> {
    pub fn new(
        keywords: &'a keywords::KeywordMap,
        ban_unknown_keywords: bool,
        supply_defaults: bool,
    ) -> CompilationSettings<'a> {
        CompilationSettings {
            keywords,
            ban_unknown_keywords,
            supply_defaults,
        }
    }
}

impl Schema {
    fn compile(
        def: Value,
        external_id: Option<url::Url>,
        settings: CompilationSettings,
    ) -> Result<Schema, SchemaError> {
        let def = helpers::convert_boolean_schema(def);

        if !def.is_object() {
            return Err(SchemaError::NotAnObject);
        }

        let id = if let Some(id) = external_id {
            id
        } else {
            helpers::parse_url_key("$id", &def)?.unwrap_or_else(helpers::generate_id)
        };

        let schema = helpers::parse_url_key("$schema", &def)?;

        let (tree, mut scopes) = {
            let mut tree = collections::BTreeMap::new();
            let obj = def.as_object().unwrap();

            let mut scopes = collections::HashMap::new();

            for (key, value) in obj.iter() {
                if !value.is_object() && !value.is_array() && !value.is_boolean() {
                    continue;
                }
                if FINAL_KEYS.contains(&key[..]) {
                    continue;
                }

                let mut context = WalkContext {
                    url: &id,
                    fragment: vec![key.clone()],
                    scopes: &mut scopes,
                };

                let scheme = Schema::compile_sub(
                    value.clone(),
                    &mut context,
                    &settings,
                    !NON_SCHEMA_KEYS.contains(&key[..]),
                )?;

                tree.insert(helpers::encode(key), scheme);
            }

            (tree, scopes)
        };

        let validators = Schema::compile_keywords(
            &def,
            &WalkContext {
                url: &id,
                fragment: vec![],
                scopes: &mut scopes,
            },
            &settings,
        )?;

        let schema = Schema {
            id: Some(id),
            schema,
            original: def,
            tree,
            validators,
            scopes,
            default: None,
        };

        Ok(schema)
    }

    pub fn add_defaults(&mut self, id: &Url, scope: &scope::Scope) {
        // step 1: walk the tree to apply this recursively, and remember child defaults
        let mut defaults = collections::BTreeMap::<&String, Value>::new();
        for (key, schema) in self.tree.iter_mut() {
            schema.add_defaults(id, scope);
            if let Some(default) = schema.default.clone() {
                defaults.insert(key, default);
            }
        }

        // step 2: use explicit default if present
        if let Some(default) = self.original.get("default") {
            self.default = Some(default.clone());
            return;
        }

        // step 3: propagate defaults according to the rules
        // 3a: $ref
        if let Some(ref_) = self.original.get("$ref").and_then(|r| r.as_str()) {
            if let Ok(url) = Url::options().base_url(Some(id)).parse(ref_) {
                if let Some(schema) = scope.resolve(&url) {
                    self.default = schema.default.clone();
                    return;
                }
            }
        }
        // 3b: properties
        if let Some(properties) = self.tree.get("properties") {
            let mut default = serde_json::Map::default();
            for (key, schema) in properties.tree.iter() {
                if let Some(value) = &schema.default {
                    default.insert(key.clone(), value.clone());
                }
            }
            if !default.is_empty() {
                self.default = Some(default.into());
                return;
            }
        }
        // 3c: items, if array
        if self
            .original
            .get("items")
            .map(|i| i.is_array())
            .unwrap_or(false)
        {
            let items = self.tree.get("items").unwrap();
            let mut default = vec![];
            let mut found_one = false;
            for idx in 0.. {
                if let Some(schema) = items.tree.get(&idx.to_string()) {
                    let def = schema
                        .default
                        .clone()
                        .map(|v| {
                            found_one = true;
                            v
                        })
                        .unwrap_or_else(|| json!({}));
                    default.push(def);
                } else {
                    break;
                }
            }
            if found_one {
                self.default = Some(default.into());
                return;
            }
        }
    }

    fn compile_keywords(
        def: &Value,
        context: &WalkContext,
        settings: &CompilationSettings,
    ) -> Result<validators::Validators, SchemaError> {
        let mut validators = vec![];
        let mut keys: collections::HashSet<&str> = def
            .as_object()
            .unwrap()
            .keys()
            .map(|key| key.as_ref())
            .collect();
        let mut not_consumed = collections::HashSet::new();

        loop {
            let key = keys.iter().next().cloned();
            if let Some(key) = key {
                match settings.keywords.get(&key) {
                    Some(keyword) => {
                        keyword.consume(&mut keys);

                        let is_exclusive_keyword = keyword.keyword.is_exclusive();

                        if let Some(validator) = keyword.keyword.compile(def, context)? {
                            if is_exclusive_keyword {
                                validators = vec![validator];
                            } else if keyword.keyword.place_first() {
                                validators.splice(0..0, std::iter::once(validator));
                            } else {
                                validators.push(validator);
                            }
                        }

                        if is_exclusive_keyword {
                            break;
                        }
                    }
                    None => {
                        keys.remove(&key);
                        if settings.ban_unknown_keywords {
                            not_consumed.insert(key);
                        }
                    }
                }
            } else {
                break;
            }
        }

        if settings.ban_unknown_keywords && !not_consumed.is_empty() {
            for key in not_consumed.iter() {
                if !ALLOW_NON_CONSUMED_KEYS.contains(&key[..]) {
                    return Err(SchemaError::UnknownKey((*key).to_string()));
                }
            }
        }

        Ok(validators)
    }

    fn compile_sub(
        def: Value,
        context: &mut WalkContext,
        keywords: &CompilationSettings,
        is_schema: bool,
    ) -> Result<Schema, SchemaError> {
        let def = helpers::convert_boolean_schema(def);

        let id = if is_schema {
            helpers::parse_url_key_with_base("$id", &def, context.url)?
        } else {
            None
        };

        let schema = if is_schema {
            helpers::parse_url_key("$schema", &def)?
        } else {
            None
        };

        let tree = {
            let mut tree = collections::BTreeMap::new();

            if def.is_object() {
                let obj = def.as_object().unwrap();
                let parent_key = &context.fragment[context.fragment.len() - 1];

                for (key, value) in obj.iter() {
                    if !value.is_object() && !value.is_array() && !value.is_boolean() {
                        continue;
                    }
                    if !PROPERTY_KEYS.contains(&parent_key[..]) && FINAL_KEYS.contains(&key[..]) {
                        continue;
                    }

                    let mut current_fragment = context.fragment.clone();
                    current_fragment.push(key.clone());

                    let is_schema = PROPERTY_KEYS.contains(&parent_key[..])
                        || !NON_SCHEMA_KEYS.contains(&key[..]);

                    let mut context = WalkContext {
                        url: id.as_ref().unwrap_or(context.url),
                        fragment: current_fragment,
                        scopes: context.scopes,
                    };

                    let scheme =
                        Schema::compile_sub(value.clone(), &mut context, keywords, is_schema)?;

                    tree.insert(helpers::encode(key), scheme);
                }
            } else if def.is_array() {
                let array = def.as_array().unwrap();
                let parent_key = &context.fragment[context.fragment.len() - 1];

                for (idx, value) in array.iter().enumerate() {
                    let mut value = value.clone();

                    if BOOLEAN_SCHEMA_ARRAY_KEYS.contains(&parent_key[..]) {
                        value = helpers::convert_boolean_schema(value);
                    }

                    if !value.is_object() && !value.is_array() {
                        continue;
                    }

                    let mut current_fragment = context.fragment.clone();
                    current_fragment.push(idx.to_string().clone());

                    let mut context = WalkContext {
                        url: id.as_ref().unwrap_or(context.url),
                        fragment: current_fragment,
                        scopes: context.scopes,
                    };

                    let scheme = Schema::compile_sub(value.clone(), &mut context, keywords, true)?;

                    tree.insert(idx.to_string().clone(), scheme);
                }
            }

            tree
        };

        if id.is_some() {
            context
                .scopes
                .insert(id.clone().unwrap().into_string(), context.fragment.clone());
        }

        let validators = if is_schema && def.is_object() {
            Schema::compile_keywords(&def, context, keywords)?
        } else {
            vec![]
        };

        let schema = Schema {
            id,
            schema,
            original: def,
            tree,
            validators,
            scopes: collections::HashMap::new(),
            default: None,
        };

        Ok(schema)
    }

    pub fn resolve(&self, id: &str) -> Option<&Schema> {
        let path = self.scopes.get(id);
        path.map(|path| {
            let mut schema = self;
            for item in path.iter() {
                schema = &schema.tree[item]
            }
            schema
        })
    }

    pub fn resolve_fragment(&self, fragment: &str) -> Option<&Schema> {
        assert!(fragment.starts_with('/'), "Can't resolve id fragments");

        let parts = fragment[1..].split('/');
        let mut schema = self;
        // FIXME what about path segments that were changed by helpers::encode()?
        for part in parts {
            match schema.tree.get(part) {
                Some(sch) => schema = sch,
                None => return None,
            }
        }

        Some(schema)
    }
}

impl Schema {
    fn validate_in_scope(
        &self,
        data: &Value,
        path: &str,
        scope: &scope::Scope,
    ) -> validators::ValidationState {
        let mut state = validators::ValidationState::new();

        for validator in self.validators.iter() {
            state.append(validator.validate(state.replacement_or(data), path, scope))
        }

        state
    }
}

pub fn compile(
    def: Value,
    external_id: Option<url::Url>,
    settings: CompilationSettings<'_>,
) -> Result<Schema, SchemaError> {
    Schema::compile(def, external_id, settings)
}

#[test]
fn schema_doesnt_compile_not_object() {
    assert!(Schema::compile(
        json!(0),
        None,
        CompilationSettings::new(&keywords::default(), true, false)
    )
    .is_err());
}

#[test]
fn schema_compiles_boolean_schema() {
    assert!(Schema::compile(
        json!(true),
        None,
        CompilationSettings::new(&keywords::default(), true, false)
    )
    .is_ok());
}
