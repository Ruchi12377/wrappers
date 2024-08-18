use pgrx::pg_sys::panic::ErrorReport;
use pgrx::prelude::PgSqlErrorCode;
use reqwest::{self, header};
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use reqwest_retry::{policies::ExponentialBackoff, RetryTransientMiddleware};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::num::ParseIntError;
use supabase_wrappers::prelude::*;
use thiserror::Error;
use yup_oauth2::AccessToken;
use yup_oauth2::ServiceAccountAuthenticator;

fn get_oauth2_token(sa_key: &str, rt: &Runtime) -> HelloWorldFdwResult<AccessToken> {
    let creds = yup_oauth2::parse_service_account_key(sa_key.as_bytes())?;
    let sa = rt.block_on(ServiceAccountAuthenticator::builder(creds).build())?;
    let scopes = &["https://www.googleapis.com/auth/spreadsheets.readonly"];
    Ok(rt.block_on(sa.token(scopes))?)
}

// A simple demo FDW
#[wrappers_fdw(
    version = "0.1.1",
    author = "Supabase",
    website = "https://github.com/supabase/wrappers/tree/main/wrappers/src/fdw/helloworld_fdw",
    error_type = "HelloWorldFdwError"
)]
pub(crate) struct HelloWorldFdw {
    rt: Runtime,
    client: Option<ClientWithMiddleware>,
    src_rows: Vec<JsonValue>,
    src_idx: usize,
}

#[derive(Error, Debug)]
enum HelloWorldFdwError {
    #[error("invalid service account key: {0}")]
    InvalidServiceAccount(#[from] std::io::Error),

    #[error("no token found in '{0:?}'")]
    NoTokenFound(yup_oauth2::AccessToken),

    #[error("get oauth2 token failed: {0}")]
    OAuthTokenError(#[from] yup_oauth2::Error),

    #[error("Firebase object '{0}' not implemented")]
    ObjectNotImplemented(String),

    #[error("column '{0}' data type is not supported")]
    UnsupportedColumnType(String),

    #[error("invalid timestamp format: {0}")]
    InvalidTimestampFormat(String),

    #[error("invalid Firebase response: {0}")]
    InvalidResponse(String),

    #[error("{0}")]
    CreateRuntimeError(#[from] CreateRuntimeError),

    #[error("{0}")]
    OptionsError(#[from] OptionsError),

    #[error("invalid api_key header")]
    InvalidApiKeyHeader,

    #[error("request failed: {0}")]
    RequestError(#[from] reqwest::Error),

    #[error("request middleware failed: {0}")]
    RequestMiddlewareError(#[from] reqwest_middleware::Error),

    #[error("`limit` option must be an integer: {0}")]
    LimitOptionParseError(#[from] ParseIntError),

    #[error("parse JSON response failed: {0}")]
    JsonParseError(#[from] serde_json::Error),
}

impl From<HelloWorldFdwError> for ErrorReport {
    fn from(value: HelloWorldFdwError) -> Self {
        ErrorReport::new(PgSqlErrorCode::ERRCODE_FDW_ERROR, format!("{value}"), "")
    }
}

type HelloWorldFdwResult<T> = Result<T, HelloWorldFdwError>;

impl ForeignDataWrapper<HelloWorldFdwError> for HelloWorldFdw {
    // 'options' is the key-value pairs defined in `CREATE SERVER` SQL, for example,
    //
    // create server my_helloworld_server
    //   foreign data wrapper wrappers_helloworld
    //   options (
    //     foo 'bar'
    // );
    //
    // 'options' passed here will be a hashmap { 'foo' -> 'bar' }.
    //
    // You can do any initalization in this new() function, like saving connection
    // info or API url in an variable, but don't do any heavy works like making a
    // database connection or API call.
    fn new(server: ForeignServer) -> HelloWorldFdwResult<Self> {
        let mut ret = Self {
            rt: create_async_runtime()?,
            client: None,
            src_rows: Vec::default(),
            src_idx: 0,
        };

        let sa_key = require_option("sa_key_id", &server.options)?.to_string();
        let access_token = get_oauth2_token(&sa_key, &ret.rt)?;
        let token = access_token
            .token()
            .map(|t| t.to_owned())
            .ok_or(HelloWorldFdwError::NoTokenFound(access_token))?;

        // header
        let mut headers = header::HeaderMap::new();
        headers.insert("user-agent", header::HeaderValue::from_static("Sheets FDW"));
        headers.insert(
            "x-datasource-auth",
            header::HeaderValue::from_static("true"),
        );
        let value = format!("Bearer {}", token);
        let mut auth_value = header::HeaderValue::from_str(&value)
            .map_err(|_| HelloWorldFdwError::InvalidApiKeyHeader)?;
        auth_value.set_sensitive(true);
        headers.insert(header::AUTHORIZATION, auth_value);

        //
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()?;
        let retry_policy = ExponentialBackoff::builder().build_with_max_retries(3);
        let client = ClientBuilder::new(client)
            .with(RetryTransientMiddleware::new_with_policy(retry_policy))
            .build();
        ret.client = Some(client);

        Ok(ret)
    }

    fn begin_scan(
        &mut self,
        _quals: &[Qual],
        columns: &[Column],
        _sorts: &[Sort],
        _limit: &Option<Limit>,
        options: &HashMap<String, String>,
    ) -> HelloWorldFdwResult<()> {
        let spread_sheet_id = require_option("spread_sheet_id", options)?.to_string();
        let sheet_id = require_option("sheet_id", options)?.to_string();

        let url = format!(
            "https://docs.google.com/spreadsheets/d/{}/gviz/tq?gid={}&tqx=out:json",
            spread_sheet_id, sheet_id,
        );
        if let Some(client) = &self.client {
            let body = self
                .rt
                .block_on(client.get(&url).send())
                .and_then(|resp| {
                    resp.error_for_status()
                        .and_then(|resp| self.rt.block_on(resp.text()))
                        .map_err(reqwest_middleware::Error::from)
                })
                .unwrap();

            let cleaned_body = body
                .strip_prefix(")]}'\n")
                .ok_or("invalid response")
                .unwrap();

            let json: JsonValue = serde_json::from_str(cleaned_body)
                .map_err(|e| e.to_string())
                .unwrap();

            self.src_rows = json;
        }

        Ok(())
    }

    fn iter_scan(&mut self, col: &mut Column) -> HelloWorldFdwResult<Option<()>> {
        // if all source rows are consumed, stop data scan
        if self.src_idx >= self.src_rows.len() {
            return Ok(None);
        }

        for tgt_col in ctx.get_columns() {
            let (tgt_col_num, tgt_col_name) = (tgt_col.num(), tgt_col.name());
            if let Some(src) = src_row.pointer(&format!("/c/{}/v", tgt_col_num - 1)) {
                // we only support I64 and String cell types here, add more type
                // conversions if you need
                let cell = match tgt_col.type_oid() {
                    TypeOid::I64 => src.as_f64().map(|v| Cell::I64(v as _)),
                    TypeOid::String => src.as_str().map(|v| Cell::String(v.to_owned())),
                    _ => {
                        return Err(format!(
                            "column {} data type is not supported",
                            tgt_col_name
                        ));
                    }
                };

                // push the cell to target row
                row.push(cell.as_ref());
            } else {
                row.push(None);
            }
        }

        // advance to next source row
        this.src_idx += 1;

        Ok(None)
    }

    fn end_scan(&mut self) -> HelloWorldFdwResult<()> {
        // we do nothing here, but you can do things like resource cleanup and etc.
        Ok(())
    }
}
