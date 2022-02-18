use std::str::FromStr;

#[cfg(feature = "json")]
use serde_json::{
    Map,
    Value,
    Number,
};

#[cfg(feature = "json")]
use serde::de::DeserializeOwned;



#[cfg(feature = "json")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathElement {
    /// A static [`String`] that must be contained in the path.
    Fixed(String),
    /// A variable part of the path with the given name.
    Variable(String),
}

impl AsRef<str> for PathElement {
    fn as_ref(&self) -> &str {
        match self {
            Self::Fixed(s) => s.as_ref(),
            Self::Variable(s) => s.as_ref(),
        }
    }
}

#[cfg(feature = "json")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathParser {
    elements: Vec<PathElement>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PathParserError {
    /// An internal variant was violated. You should inform the developer if you encounter this error.
    Internal,
    /// Deserializing with the help of [`serde_json`] failed.
    JsonParseError,
    /// A path element which did not end in a slash contained a slash, which is not permitted.
    InvalidSlash,
    /// The parser expected more path elements, but the provided path did not contain them.
    UnexpectedEndOfPath,
}

impl PathParser {
    pub fn elements(&self) -> &Vec<PathElement> {
        &self.elements
    }

    pub fn parse<T>(&self, path: impl AsRef<str>) -> Result<T, PathParserError>
    where
        T: DeserializeOwned,
    {
        let mut path = path.as_ref();

        let mut map = Map::with_capacity(self.elements.len()/2);

        for idx in 0..self.elements.len() {
            if path.is_empty() {
                break; //  Abort early.
            }

            let element = self.elements.get(idx).ok_or_else(|| PathParserError::Internal)?;

            match element {
                PathElement::Fixed(prefix) => path = path.strip_prefix(prefix).ok_or_else(|| PathParserError::Internal)?,
                PathElement::Variable(name) => {
                    // Initialize the store for the variable path component.
                    // Example: "/{var}" <=> "/test" => prefix = "test"
                    let mut prefix = String::with_capacity(64);

                    match self.elements.get(idx+1) {
                        // If there is another element, make sure we find the
                        // proper end.
                        Some(element) => {
                            let search: &str = element.as_ref();
                            let search = search.chars().next().ok_or_else(|| PathParserError::Internal)?;

                            let mut end_found = false;

                            for ch in path.chars() {
                                if ch==search {
                                    end_found = true;
                                    break;
                                }

                                if ch=='/' {
                                    // Invalid end of the variable element.
                                    return Err(PathParserError::InvalidSlash);
                                }

                                prefix.push(ch);
                            }

                            if !end_found {
                                return Err(PathParserError::UnexpectedEndOfPath);
                            }
                        }
                        // If this is the last element, read to the end or until
                        // we find a slash.
                        None => {
                            for ch in path.chars() {
                                if ch=='/' {
                                    break;
                                }

                                prefix.push(ch);
                            }
                        }
                    }

                    path = path.strip_prefix(&prefix).ok_or_else(|| PathParserError::Internal)?;

                    map.insert(name.into(), Self::string_to_json_value(prefix));
                }
            }
        }

        serde_json::from_value(Value::Object(map)).map_err(|err| {
            log::error!("PathParser: Parsing as json failed. err: {:?}", err);

            PathParserError::JsonParseError
        })
    }

    fn string_to_json_value(s: impl AsRef<str>) -> Value {
        let s = s.as_ref();

        if s=="null" {
            return Value::Null;
        }
        if s=="true" {
            return Value::Bool(true);
        }
        if s=="false" {
            return Value::Bool(false);
        }
        if let Ok(number) = Number::from_str(s) {
            return Value::Number(number);
        }

        Value::String(s.into())
    }
}

impl FromStr for PathParser {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.chars().nth(0).ok_or_else(|| ())?!='/' {
            return Err(());
        }

        let mut elements = Vec::new();

        let mut element = String::new();
        let mut fixed = true;
        for ch in s.chars() {
            if fixed {
                if ch=='}' {
                    return Err(());
                }

                if ch=='{' {
                    if element.is_empty() {
                        return Err(());
                    }
                    elements.push(PathElement::Fixed(element));
                    element = String::new();
                    fixed = false;
                    continue;
                }
            } else {
                if ch=='{' {
                    return Err(());
                }

                if ch=='/' {
                    return Err(());
                }

                if ch=='}' {
                    if element.is_empty() {
                        return Err(());
                    }
                    for compare in elements.iter() {
                        if compare.as_ref()==element {
                            return Err(());
                        }
                    }
                    elements.push(PathElement::Variable(element));
                    element = String::new();
                    fixed = true;
                    continue;
                }
            }

            element.push(ch);
        }

        if !fixed {
            return Err(());
        }

        if !element.is_empty() {
            elements.push(PathElement::Fixed(element));
        }

        Ok(Self {
            elements,
        })
    }
}



#[cfg(test)]
mod tests {
    use super::*;

    use serde::Deserialize;

    #[test]
    fn path_parser_parse_fail_on_empty_string() {
        assert!(PathParser::from_str("").is_err());
    }

    #[test]
    fn path_parser_parse_fail_on_no_slash_at_beginning() {
        assert!(PathParser::from_str("s").is_err());
    }

    #[test]
    fn path_parser_parse_fail_on_non_closed_variable() {
        assert!(PathParser::from_str("/{").is_err());
        assert!(PathParser::from_str("/{lol").is_err());
        assert!(PathParser::from_str("/{lol}{").is_err());
        assert!(PathParser::from_str("/{lol}/{").is_err());
        assert!(PathParser::from_str("/{lol}/folder{").is_err());
        assert!(PathParser::from_str("/{lol}/folder/{").is_err());
    }

    #[test]
    fn path_parser_parse_fail_on_not_opened_variable() {
        assert!(PathParser::from_str("/}").is_err());
        assert!(PathParser::from_str("/lol}").is_err());
        assert!(PathParser::from_str("/lol/}").is_err());
        assert!(PathParser::from_str("/{lol}/folder}").is_err());
        assert!(PathParser::from_str("/{lol}/folder/}").is_err());
    }

    #[test]
    fn path_parser_parse_fail_on_doubly_opened_variable() {
        assert!(PathParser::from_str("/{{").is_err());
        assert!(PathParser::from_str("/{{}").is_err());
        assert!(PathParser::from_str("/{lol{").is_err());
        assert!(PathParser::from_str("/{lol{}").is_err());
        assert!(PathParser::from_str("/{lol{path}").is_err());
        assert!(PathParser::from_str("/{lol{path}}").is_err());
        assert!(PathParser::from_str("/{lol}/folder{{}").is_err());
        assert!(PathParser::from_str("/{lol}/folder/{{}").is_err());
    }

    #[test]
    fn path_parser_parse_fail_on_no_seperator() {
        assert!(PathParser::from_str("/{var1}{var2}").is_err());
        assert!(PathParser::from_str("/path{var1}{var2}").is_err());
        assert!(PathParser::from_str("/{var1}/{var2}{var3}").is_err());
        assert!(PathParser::from_str("/{var1}/path{var2}{var3}").is_err());
        assert!(PathParser::from_str("/{var1}/path/{var2}{var3}").is_err());
    }

    #[test]
    fn path_parser_parse_fail_on_empty_variable_name() {
        assert!(PathParser::from_str("/{}").is_err());
        assert!(PathParser::from_str("/{var1}{}").is_err());
        assert!(PathParser::from_str("/{}{var2}").is_err());
        assert!(PathParser::from_str("/path{}{var2}").is_err());
        assert!(PathParser::from_str("/{var1}/{}{var3}").is_err());
        assert!(PathParser::from_str("/{var1}/path{}{var3}").is_err());
        assert!(PathParser::from_str("/{var1}/path/{}{}").is_err());
    }

    #[test]
    fn path_parser_parse_fail_on_double_variable_name() {
        assert!(PathParser::from_str("/{var1}{var1}").is_err());
        assert!(PathParser::from_str("/path{var1}{var1}").is_err());
        assert!(PathParser::from_str("/{var1}/{var2}{var1}").is_err());
        assert!(PathParser::from_str("/{var1}/path{var1}{var3}").is_err());
        assert!(PathParser::from_str("/{var1}/path/{var2}{var1}").is_err());
    }

    #[test]
    fn path_parser_parse_fail_on_variable_name_with_slash() {
        assert!(PathParser::from_str("/{var/1}{var2}").is_err());
        assert!(PathParser::from_str("/{var1}{va/r2}").is_err());
        assert!(PathParser::from_str("/{var/1}{va/r2}").is_err());
        assert!(PathParser::from_str("/path{va/r1}{var2}").is_err());
        assert!(PathParser::from_str("/path{var1}{va/r2}").is_err());
        assert!(PathParser::from_str("/path{va/r1}{va/r2}").is_err());
        assert!(PathParser::from_str("/{va/r1}/{var2}{var3}").is_err());
        assert!(PathParser::from_str("/{var1}/path{var2/}{var3}").is_err());
        assert!(PathParser::from_str("/{var1}/path/{var2}{/var3}").is_err());
    }

    #[test]
    fn path_parser_parse_fixed() {
        let parser = PathParser::from_str("/home/user1").unwrap();

        let elements = vec![
            PathElement::Fixed("/home/user1".into()),
        ];

        let compare = PathParser{elements};

        assert_eq!(parser, compare);

        assert!(PathParser::from_str("/").is_ok());
    }

    #[test]
    fn path_parser_parse_simple() {
        let parser = PathParser::from_str("/home/{user}").unwrap();

        let elements = vec![
            PathElement::Fixed("/home/".into()),
            PathElement::Variable("user".into())
        ];

        let compare = PathParser{elements};

        assert_eq!(parser, compare);
    }

    #[test]
    fn path_parser_parse_long() {
        let parser = PathParser::from_str("/home/{user}/folder{folderid}").unwrap();

        let elements = vec![
            PathElement::Fixed("/home/".into()),
            PathElement::Variable("user".into()),
            PathElement::Fixed("/folder".into()),
            PathElement::Variable("folderid".into()),
        ];

        let compare = PathParser{elements};

        assert_eq!(parser, compare);
    }

    #[derive(Deserialize, Debug, PartialEq, Eq)]
    struct PathParserTest {
        user: String,
        folderid: u64,
    }

    #[test]
    fn path_parser_parse_path() {
        let parser = PathParser::from_str("/home/{user}/folder{folderid}").unwrap();

        let parsed: PathParserTest = parser.parse("/home/alfred/folder123").unwrap();

        let compare = PathParserTest {
            user: "alfred".into(),
            folderid: 123,
        };

        assert_eq!(parsed, compare);
    }

    #[derive(Deserialize, Debug, PartialEq, Eq)]
    struct PathParserTest1 {
        path: String,
        user: u64,
        active: bool,
        banned: bool,
        null: Option<i32>,
        rest: String,
    }

    #[test]
    fn path_parser_parse_path_full() {
        let parser = PathParser::from_str("/{path}/u{user}/{active}_{banned}0{null}/{rest}").unwrap();

        let parsed: PathParserTest1 = parser.parse("/home/u123/true_false0null/folder/file").unwrap();

        let compare = PathParserTest1 {
            path: "home".into(),
            user: 123,
            active: true,
            banned: false,
            null: None,
            rest: "folder".into(),
        };

        assert_eq!(parsed, compare);
    }
}
