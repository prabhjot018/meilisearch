use std::cmp::min;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::str::FromStr;
use std::time::Instant;

use either::Either;
use milli::tokenizer::{Analyzer, AnalyzerConfig};
use milli::{
    AscDesc, FieldId, FieldsIdsMap, Filter, FormatOptions, MatchBounds, MatcherBuilder, SortError,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::index::error::FacetError;

use super::error::{IndexError, Result};
use super::index::Index;

pub type Document = serde_json::Map<String, Value>;
type MatchesPosition = BTreeMap<String, Vec<MatchBounds>>;

pub const DEFAULT_SEARCH_LIMIT: fn() -> usize = || 20;
pub const DEFAULT_CROP_LENGTH: fn() -> usize = || 10;
pub const DEFAULT_CROP_MARKER: fn() -> String = || "…".to_string();
pub const DEFAULT_HIGHLIGHT_PRE_TAG: fn() -> String = || "<em>".to_string();
pub const DEFAULT_HIGHLIGHT_POST_TAG: fn() -> String = || "</em>".to_string();

/// The maximimum number of results that the engine
/// will be able to return in one search call.
pub const HARD_RESULT_LIMIT: usize = 1000;

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchQuery {
    pub q: Option<String>,
    pub offset: Option<usize>,
    #[serde(default = "DEFAULT_SEARCH_LIMIT")]
    pub limit: usize,
    pub attributes_to_retrieve: Option<BTreeSet<String>>,
    pub attributes_to_crop: Option<Vec<String>>,
    #[serde(default = "DEFAULT_CROP_LENGTH")]
    pub crop_length: usize,
    pub attributes_to_highlight: Option<HashSet<String>>,
    // Default to false
    #[serde(default = "Default::default")]
    pub show_matches_position: bool,
    pub filter: Option<Value>,
    pub sort: Option<Vec<String>>,
    pub facets: Option<Vec<String>>,
    #[serde(default = "DEFAULT_HIGHLIGHT_PRE_TAG")]
    pub highlight_pre_tag: String,
    #[serde(default = "DEFAULT_HIGHLIGHT_POST_TAG")]
    pub highlight_post_tag: String,
    #[serde(default = "DEFAULT_CROP_MARKER")]
    pub crop_marker: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SearchHit {
    #[serde(flatten)]
    pub document: Document,
    #[serde(rename = "_formatted", skip_serializing_if = "Document::is_empty")]
    pub formatted: Document,
    #[serde(rename = "_matchesPosition", skip_serializing_if = "Option::is_none")]
    pub matches_position: Option<MatchesPosition>,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
    pub estimated_total_hits: u64,
    pub query: String,
    pub limit: usize,
    pub offset: usize,
    pub processing_time_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facet_distribution: Option<BTreeMap<String, BTreeMap<String, u64>>>,
}

impl Index {
    pub fn perform_search(&self, query: SearchQuery) -> Result<SearchResult> {
        let before_search = Instant::now();
        let rtxn = self.read_txn()?;

        let mut search = self.search(&rtxn);

        if let Some(ref query) = query.q {
            search.query(query);
        }

        // Make sure that a user can't get more documents than the hard limit,
        // we align that on the offset too.
        let offset = min(query.offset.unwrap_or(0), HARD_RESULT_LIMIT);
        let limit = min(query.limit, HARD_RESULT_LIMIT.saturating_sub(offset));

        search.offset(offset);
        search.limit(limit);

        if let Some(ref filter) = query.filter {
            if let Some(facets) = parse_filter(filter)? {
                search.filter(facets);
            }
        }

        if let Some(ref sort) = query.sort {
            let sort = match sort.iter().map(|s| AscDesc::from_str(s)).collect() {
                Ok(sorts) => sorts,
                Err(asc_desc_error) => {
                    return Err(IndexError::Milli(SortError::from(asc_desc_error).into()))
                }
            };

            search.sort_criteria(sort);
        }

        let milli::SearchResult {
            documents_ids,
            matching_words,
            candidates,
            ..
        } = search.execute()?;

        let fields_ids_map = self.fields_ids_map(&rtxn).unwrap();

        let displayed_ids = self
            .displayed_fields_ids(&rtxn)?
            .map(|fields| fields.into_iter().collect::<BTreeSet<_>>())
            .unwrap_or_else(|| fields_ids_map.iter().map(|(id, _)| id).collect());

        let fids = |attrs: &BTreeSet<String>| {
            let mut ids = BTreeSet::new();
            for attr in attrs {
                if attr == "*" {
                    ids = displayed_ids.clone();
                    break;
                }

                if let Some(id) = fields_ids_map.id(attr) {
                    ids.insert(id);
                }
            }
            ids
        };

        // The attributes to retrieve are the ones explicitly marked as to retrieve (all by default),
        // but these attributes must be also be present
        // - in the fields_ids_map
        // - in the the displayed attributes
        let to_retrieve_ids: BTreeSet<_> = query
            .attributes_to_retrieve
            .as_ref()
            .map(fids)
            .unwrap_or_else(|| displayed_ids.clone())
            .intersection(&displayed_ids)
            .cloned()
            .collect();

        let attr_to_highlight = query.attributes_to_highlight.unwrap_or_default();

        let attr_to_crop = query.attributes_to_crop.unwrap_or_default();

        // Attributes in `formatted_options` correspond to the attributes that will be in `_formatted`
        // These attributes are:
        // - the attributes asked to be highlighted or cropped (with `attributesToCrop` or `attributesToHighlight`)
        // - the attributes asked to be retrieved: these attributes will not be highlighted/cropped
        // But these attributes must be also present in displayed attributes
        let formatted_options = compute_formatted_options(
            &attr_to_highlight,
            &attr_to_crop,
            query.crop_length,
            &to_retrieve_ids,
            &fields_ids_map,
            &displayed_ids,
        );

        let stop_words = fst::Set::default();
        let mut config = AnalyzerConfig::default();
        config.stop_words(&stop_words);
        let analyzer = Analyzer::new(config);

        let mut formatter_builder = MatcherBuilder::from_matching_words(matching_words);
        formatter_builder.crop_marker(query.crop_marker);
        formatter_builder.highlight_prefix(query.highlight_pre_tag);
        formatter_builder.highlight_suffix(query.highlight_post_tag);

        let mut documents = Vec::new();

        let documents_iter = self.documents(&rtxn, documents_ids)?;

        for (_id, obkv) in documents_iter {
            // First generate a document with all the displayed fields
            let displayed_document = make_document(&displayed_ids, &fields_ids_map, obkv)?;

            // select the attributes to retrieve
            let attributes_to_retrieve = to_retrieve_ids
                .iter()
                .map(|&fid| fields_ids_map.name(fid).expect("Missing field name"));
            let mut document =
                permissive_json_pointer::select_values(&displayed_document, attributes_to_retrieve);

            let (matches_position, formatted) = format_fields(
                &displayed_document,
                &fields_ids_map,
                &formatter_builder,
                &analyzer,
                &formatted_options,
                query.show_matches_position,
                &displayed_ids,
            )?;

            if let Some(sort) = query.sort.as_ref() {
                insert_geo_distance(sort, &mut document);
            }

            let hit = SearchHit {
                document,
                formatted,
                matches_position,
            };
            documents.push(hit);
        }

        let estimated_total_hits = candidates.len();

        let facet_distribution = match query.facets {
            Some(ref fields) => {
                let mut facet_distribution = self.facets_distribution(&rtxn);
                if fields.iter().all(|f| f != "*") {
                    facet_distribution.facets(fields);
                }
                let distribution = facet_distribution.candidates(candidates).execute()?;

                Some(distribution)
            }
            None => None,
        };

        let result = SearchResult {
            hits: documents,
            estimated_total_hits,
            query: query.q.clone().unwrap_or_default(),
            limit: query.limit,
            offset: query.offset.unwrap_or_default(),
            processing_time_ms: before_search.elapsed().as_millis(),
            facet_distribution,
        };
        Ok(result)
    }
}

fn insert_geo_distance(sorts: &[String], document: &mut Document) {
    lazy_static::lazy_static! {
        static ref GEO_REGEX: Regex =
            Regex::new(r"_geoPoint\(\s*([[:digit:].\-]+)\s*,\s*([[:digit:].\-]+)\s*\)").unwrap();
    };
    if let Some(capture_group) = sorts.iter().find_map(|sort| GEO_REGEX.captures(sort)) {
        // TODO: TAMO: milli encountered an internal error, what do we want to do?
        let base = [
            capture_group[1].parse().unwrap(),
            capture_group[2].parse().unwrap(),
        ];
        let geo_point = &document.get("_geo").unwrap_or(&json!(null));
        if let Some((lat, lng)) = geo_point["lat"].as_f64().zip(geo_point["lng"].as_f64()) {
            let distance = milli::distance_between_two_points(&base, &[lat, lng]);
            document.insert("_geoDistance".to_string(), json!(distance.round() as usize));
        }
    }
}

fn compute_formatted_options(
    attr_to_highlight: &HashSet<String>,
    attr_to_crop: &[String],
    query_crop_length: usize,
    to_retrieve_ids: &BTreeSet<FieldId>,
    fields_ids_map: &FieldsIdsMap,
    displayed_ids: &BTreeSet<FieldId>,
) -> BTreeMap<FieldId, FormatOptions> {
    let mut formatted_options = BTreeMap::new();

    add_highlight_to_formatted_options(
        &mut formatted_options,
        attr_to_highlight,
        fields_ids_map,
        displayed_ids,
    );

    add_crop_to_formatted_options(
        &mut formatted_options,
        attr_to_crop,
        query_crop_length,
        fields_ids_map,
        displayed_ids,
    );

    // Should not return `_formatted` if no valid attributes to highlight/crop
    if !formatted_options.is_empty() {
        add_non_formatted_ids_to_formatted_options(&mut formatted_options, to_retrieve_ids);
    }

    formatted_options
}

fn add_highlight_to_formatted_options(
    formatted_options: &mut BTreeMap<FieldId, FormatOptions>,
    attr_to_highlight: &HashSet<String>,
    fields_ids_map: &FieldsIdsMap,
    displayed_ids: &BTreeSet<FieldId>,
) {
    for attr in attr_to_highlight {
        let new_format = FormatOptions {
            highlight: true,
            crop: None,
        };

        if attr == "*" {
            for id in displayed_ids {
                formatted_options.insert(*id, new_format);
            }
            break;
        }

        if let Some(id) = fields_ids_map.id(attr) {
            if displayed_ids.contains(&id) {
                formatted_options.insert(id, new_format);
            }
        }
    }
}

fn add_crop_to_formatted_options(
    formatted_options: &mut BTreeMap<FieldId, FormatOptions>,
    attr_to_crop: &[String],
    crop_length: usize,
    fields_ids_map: &FieldsIdsMap,
    displayed_ids: &BTreeSet<FieldId>,
) {
    for attr in attr_to_crop {
        let mut split = attr.rsplitn(2, ':');
        let (attr_name, attr_len) = match split.next().zip(split.next()) {
            Some((len, name)) => {
                let crop_len = len.parse::<usize>().unwrap_or(crop_length);
                (name, crop_len)
            }
            None => (attr.as_str(), crop_length),
        };

        if attr_name == "*" {
            for id in displayed_ids {
                formatted_options
                    .entry(*id)
                    .and_modify(|f| f.crop = Some(attr_len))
                    .or_insert(FormatOptions {
                        highlight: false,
                        crop: Some(attr_len),
                    });
            }
        }

        if let Some(id) = fields_ids_map.id(attr_name) {
            if displayed_ids.contains(&id) {
                formatted_options
                    .entry(id)
                    .and_modify(|f| f.crop = Some(attr_len))
                    .or_insert(FormatOptions {
                        highlight: false,
                        crop: Some(attr_len),
                    });
            }
        }
    }
}

fn add_non_formatted_ids_to_formatted_options(
    formatted_options: &mut BTreeMap<FieldId, FormatOptions>,
    to_retrieve_ids: &BTreeSet<FieldId>,
) {
    for id in to_retrieve_ids {
        formatted_options.entry(*id).or_insert(FormatOptions {
            highlight: false,
            crop: None,
        });
    }
}

fn make_document(
    displayed_attributes: &BTreeSet<FieldId>,
    field_ids_map: &FieldsIdsMap,
    obkv: obkv::KvReaderU16,
) -> Result<Document> {
    let mut document = serde_json::Map::new();

    // recreate the original json
    for (key, value) in obkv.iter() {
        let value = serde_json::from_slice(value)?;
        let key = field_ids_map
            .name(key)
            .expect("Missing field name")
            .to_string();

        document.insert(key, value);
    }

    // select the attributes to retrieve
    let displayed_attributes = displayed_attributes
        .iter()
        .map(|&fid| field_ids_map.name(fid).expect("Missing field name"));

    let document = permissive_json_pointer::select_values(&document, displayed_attributes);
    Ok(document)
}

fn format_fields<'a, A: AsRef<[u8]>>(
    document: &Document,
    field_ids_map: &FieldsIdsMap,
    builder: &MatcherBuilder,
    analyzer: &'a Analyzer<'a, A>,
    formatted_options: &BTreeMap<FieldId, FormatOptions>,
    compute_matches: bool,
    displayable_ids: &BTreeSet<FieldId>,
) -> Result<(Option<MatchesPosition>, Document)> {
    let mut matches_position = compute_matches.then(BTreeMap::new);
    let mut document = document.clone();

    // select the attributes to retrieve
    let displayable_names = displayable_ids
        .iter()
        .map(|&fid| field_ids_map.name(fid).expect("Missing field name"));
    permissive_json_pointer::map_leaf_values(&mut document, displayable_names, |key, value| {
        // To get the formatting option of each key we need to see all the rules that applies
        // to the value and merge them together. eg. If a user said he wanted to highlight `doggo`
        // and crop `doggo.name`. `doggo.name` needs to be highlighted + cropped while `doggo.age` is only
        // highlighted.
        let format = formatted_options
            .iter()
            .filter(|(field, _option)| {
                let name = field_ids_map.name(**field).unwrap();
                milli::is_faceted_by(name, key) || milli::is_faceted_by(key, name)
            })
            .map(|(_, option)| *option)
            .reduce(|acc, option| acc.merge(option));
        let mut infos = Vec::new();

        *value = format_value(
            std::mem::take(value),
            builder,
            format,
            analyzer,
            &mut infos,
            compute_matches,
        );

        if let Some(matches) = matches_position.as_mut() {
            if !infos.is_empty() {
                matches.insert(key.to_owned(), infos);
            }
        }
    });

    let selectors = formatted_options
        .keys()
        // This unwrap must be safe since we got the ids from the fields_ids_map just
        // before.
        .map(|&fid| field_ids_map.name(fid).unwrap());
    let document = permissive_json_pointer::select_values(&document, selectors);

    Ok((matches_position, document))
}

fn format_value<'a, A: AsRef<[u8]>>(
    value: Value,
    builder: &MatcherBuilder,
    format_options: Option<FormatOptions>,
    analyzer: &'a Analyzer<'a, A>,
    infos: &mut Vec<MatchBounds>,
    compute_matches: bool,
) -> Value {
    match value {
        Value::String(old_string) => {
            // this will be removed with charabia
            let analyzed = analyzer.analyze(&old_string);
            let tokens: Vec<_> = analyzed.tokens().collect();

            let mut matcher = builder.build(&tokens[..], &old_string);
            if compute_matches {
                let matches = matcher.matches();
                infos.extend_from_slice(&matches[..]);
            }

            match format_options {
                Some(format_options) => {
                    let value = matcher.format(format_options);
                    Value::String(value.into_owned())
                }
                None => Value::String(old_string),
            }
        }
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|v| {
                    format_value(
                        v,
                        builder,
                        format_options.map(|format_options| FormatOptions {
                            highlight: format_options.highlight,
                            crop: None,
                        }),
                        analyzer,
                        infos,
                        compute_matches,
                    )
                })
                .collect(),
        ),
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(k, v)| {
                    (
                        k,
                        format_value(
                            v,
                            builder,
                            format_options.map(|format_options| FormatOptions {
                                highlight: format_options.highlight,
                                crop: None,
                            }),
                            analyzer,
                            infos,
                            compute_matches,
                        ),
                    )
                })
                .collect(),
        ),
        Value::Number(number) => {
            // this will be removed with charabia
            let s = number.to_string();
            let analyzed = analyzer.analyze(&s);
            let tokens: Vec<_> = analyzed.tokens().collect();

            let mut matcher = builder.build(&tokens[..], &s);
            if compute_matches {
                let matches = matcher.matches();
                infos.extend_from_slice(&matches[..]);
            }

            match format_options {
                Some(format_options) => {
                    let value = matcher.format(format_options);
                    Value::String(value.into_owned())
                }
                None => Value::Number(number),
            }
        }
        value => value,
    }
}

fn parse_filter(facets: &Value) -> Result<Option<Filter>> {
    match facets {
        Value::String(expr) => {
            let condition = Filter::from_str(expr)?;
            Ok(condition)
        }
        Value::Array(arr) => parse_filter_array(arr),
        v => Err(FacetError::InvalidExpression(&["Array"], v.clone()).into()),
    }
}

fn parse_filter_array(arr: &[Value]) -> Result<Option<Filter>> {
    let mut ands = Vec::new();
    for value in arr {
        match value {
            Value::String(s) => ands.push(Either::Right(s.as_str())),
            Value::Array(arr) => {
                let mut ors = Vec::new();
                for value in arr {
                    match value {
                        Value::String(s) => ors.push(s.as_str()),
                        v => {
                            return Err(FacetError::InvalidExpression(&["String"], v.clone()).into())
                        }
                    }
                }
                ands.push(Either::Left(ors));
            }
            v => {
                return Err(
                    FacetError::InvalidExpression(&["String", "[String]"], v.clone()).into(),
                )
            }
        }
    }

    Ok(Filter::from_array(ands)?)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_insert_geo_distance() {
        let value: Document = serde_json::from_str(
            r#"{
              "_geo": {
                "lat": 50.629973371633746,
                "lng": 3.0569447399419567
              },
              "city": "Lille",
              "id": "1"
            }"#,
        )
        .unwrap();

        let sorters = &["_geoPoint(50.629973371633746,3.0569447399419567):desc".to_string()];
        let mut document = value.clone();
        insert_geo_distance(sorters, &mut document);
        assert_eq!(document.get("_geoDistance"), Some(&json!(0)));

        let sorters = &["_geoPoint(50.629973371633746, 3.0569447399419567):asc".to_string()];
        let mut document = value.clone();
        insert_geo_distance(sorters, &mut document);
        assert_eq!(document.get("_geoDistance"), Some(&json!(0)));

        let sorters =
            &["_geoPoint(   50.629973371633746   ,  3.0569447399419567   ):desc".to_string()];
        let mut document = value.clone();
        insert_geo_distance(sorters, &mut document);
        assert_eq!(document.get("_geoDistance"), Some(&json!(0)));

        let sorters = &[
            "prix:asc",
            "villeneuve:desc",
            "_geoPoint(50.629973371633746, 3.0569447399419567):asc",
            "ubu:asc",
        ]
        .map(|s| s.to_string());
        let mut document = value.clone();
        insert_geo_distance(sorters, &mut document);
        assert_eq!(document.get("_geoDistance"), Some(&json!(0)));

        // only the first geoPoint is used to compute the distance
        let sorters = &[
            "chien:desc",
            "_geoPoint(50.629973371633746, 3.0569447399419567):asc",
            "pangolin:desc",
            "_geoPoint(100.0, -80.0):asc",
            "chat:asc",
        ]
        .map(|s| s.to_string());
        let mut document = value.clone();
        insert_geo_distance(sorters, &mut document);
        assert_eq!(document.get("_geoDistance"), Some(&json!(0)));

        // there was no _geoPoint so nothing is inserted in the document
        let sorters = &["chien:asc".to_string()];
        let mut document = value;
        insert_geo_distance(sorters, &mut document);
        assert_eq!(document.get("_geoDistance"), None);
    }
}
