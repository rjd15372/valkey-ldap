use lazy_static::lazy_static;
use ldap3::{LdapConn, LdapConnSettings, LdapError, Scope, SearchEntry};
use native_tls::{Certificate, Identity, TlsConnector};
use std::{collections::LinkedList, fs, sync::Mutex};
use url::Url;
use valkey_module::ValkeyError;

use crate::configs::LdapSearchScope;

struct VkLdapConfig {
    servers: LinkedList<Url>,
}

impl VkLdapConfig {
    fn new() -> VkLdapConfig {
        VkLdapConfig {
            servers: LinkedList::new(),
        }
    }

    fn clear_server_list(&mut self) -> () {
        self.servers.clear();
    }

    fn add_server(&mut self, server_url: Url) -> () {
        self.servers.push_back(server_url);
    }

    pub fn find_server(&self) -> Option<&Url> {
        self.servers.front()
    }
}

lazy_static! {
    static ref LDAP_CONFIG: Mutex<VkLdapConfig> = Mutex::new(VkLdapConfig::new());
}

pub fn clear_server_list() -> () {
    LDAP_CONFIG.lock().unwrap().clear_server_list();
}

pub fn add_server(server_url: Url) {
    LDAP_CONFIG.lock().unwrap().add_server(server_url);
}

pub enum VkLdapError {
    IOError(String, std::io::Error),
    NoTLSKeyPathSet,
    TLSError(String, native_tls::Error),
    LdapBindError(LdapError),
    LdapAdminBindError(LdapError),
    LdapSearchError(LdapError),
    LdapCreateContextError(LdapError),
    NoLdapEntryFound(String),
    MultipleEntryFound(String),
    NoServerConfigured,
}

impl std::fmt::Display for VkLdapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VkLdapError::NoTLSKeyPathSet => write!(
                f,
                "no TLS key path specified. Please set the path for ldap.tls_key_path config"
            ),
            VkLdapError::IOError(msg, ioerr) => write!(f, "{msg}: {ioerr}"),
            VkLdapError::TLSError(msg, tlserr) => write!(f, "{msg}: {tlserr}"),
            VkLdapError::LdapBindError(ldaperr) => {
                write!(f, "error in bind operation: {ldaperr}")
            }
            VkLdapError::LdapAdminBindError(ldaperr) => {
                write!(f, "error in binding admin user: {ldaperr}")
            }
            VkLdapError::LdapSearchError(ldaperr) => {
                write!(f, "failed to search ldap user: {ldaperr}")
            }
            VkLdapError::LdapCreateContextError(ldaperr) => {
                write!(f, "failed to create LDAP connection context: {ldaperr}")
            }
            VkLdapError::NoLdapEntryFound(filter) => {
                write!(f, "search filter '{filter}' returned no entries")
            }
            VkLdapError::MultipleEntryFound(filter) => {
                write!(f, "search filter '{filter}' returned multiple entries")
            }
            VkLdapError::NoServerConfigured => write!(
                f,
                "no server set in configuration. Please set ldap.servers config option"
            ),
        }
    }
}

impl From<&VkLdapError> for ValkeyError {
    fn from(err: &VkLdapError) -> Self {
        err.into()
    }
}

macro_rules! handle_io_error {
    ($expr:expr, $errmsg:expr) => {
        match $expr {
            Ok(res) => res,
            Err(err) => return Err(VkLdapError::IOError($errmsg, err)),
        }
    };
}

macro_rules! handle_tls_error {
    ($expr:expr, $errmsg:expr) => {
        match $expr {
            Ok(res) => res,
            Err(err) => return Err(VkLdapError::TLSError($errmsg, err)),
        }
    };
}

macro_rules! handle_ldap_error {
    ($expr:expr, $errtype:expr) => {
        match $expr {
            Ok(res) => match res.success() {
                Ok(res) => res,
                Err(err) => return Err($errtype(err)),
            },
            Err(err) => return Err($errtype(err)),
        }
    };
}

type Result<T> = std::result::Result<T, VkLdapError>;

impl From<LdapSearchScope> for Scope {
    fn from(value: LdapSearchScope) -> Self {
        match value {
            LdapSearchScope::Base => Scope::Base,
            LdapSearchScope::OneLevel => Scope::OneLevel,
            LdapSearchScope::SubTree => Scope::Subtree,
        }
    }
}

pub struct VkLdapSettings {
    use_starttls: bool,
    ca_cert_path: Option<String>,
    client_cert_path: Option<String>,
    client_key_path: Option<String>,
    bind_db_prefix: String,
    bind_db_suffix: String,
    search_base: Option<String>,
    search_scope: Scope,
    search_filter: Option<String>,
    search_attribute: Option<String>,
    search_bind_dn: Option<String>,
    search_bind_passwd: Option<String>,
    search_dn_attribute: String,
}

impl VkLdapSettings {
    pub fn new(
        use_starttls: bool,
        ca_cert_path: Option<String>,
        client_cert_path: Option<String>,
        client_key_path: Option<String>,
        bind_db_prefix: String,
        bind_db_suffix: String,
        search_base: Option<String>,
        search_scope: LdapSearchScope,
        search_filter: Option<String>,
        search_attribute: Option<String>,
        search_bind_dn: Option<String>,
        search_bind_passwd: Option<String>,
        search_dn_attribute: String,
    ) -> Self {
        Self {
            use_starttls,
            ca_cert_path,
            client_cert_path,
            client_key_path,
            bind_db_prefix,
            bind_db_suffix,
            search_base,
            search_scope: search_scope.into(),
            search_filter,
            search_attribute,
            search_bind_dn,
            search_bind_passwd,
            search_dn_attribute,
        }
    }
}

struct VkLdapContext {
    ldap_conn: LdapConn,
    settings: VkLdapSettings,
}

impl VkLdapContext {
    fn new(settings: VkLdapSettings, url: &Url) -> Result<Self> {
        let mut ldap_conn_settings = LdapConnSettings::new();

        let use_starttls = settings.use_starttls;
        let requires_tls = url.scheme() == "ldaps" || use_starttls;

        if requires_tls {
            let mut tls_builder = &mut TlsConnector::builder();

            if let Some(path) = &settings.ca_cert_path {
                let ca_cert_bytes =
                    handle_io_error!(fs::read(path), "failed to read CA cert file".to_string());
                let ca_cert = handle_tls_error!(
                    Certificate::from_pem(&ca_cert_bytes),
                    "failed to load CA certificate".to_string()
                );
                tls_builder = tls_builder.add_root_certificate(ca_cert);
            }

            if let Some(cert_path) = &settings.client_cert_path {
                match &settings.client_key_path {
                    None => return Err(VkLdapError::NoTLSKeyPathSet),
                    Some(key_path) => {
                        let cert_bytes = handle_io_error!(
                            fs::read(cert_path),
                            "failed to read client certificate file".to_string()
                        );
                        let key_bytes = handle_io_error!(
                            fs::read(key_path),
                            "failed to read client key file".to_string()
                        );
                        let client_cert = handle_tls_error!(
                            Identity::from_pkcs8(&cert_bytes, &key_bytes),
                            "failed to load client certificate".to_string()
                        );
                        tls_builder = tls_builder.identity(client_cert);
                    }
                }
            }

            let tls_connector = handle_tls_error!(
                tls_builder.build(),
                "failed to setup TLS connection".to_string()
            );

            ldap_conn_settings = ldap_conn_settings.set_connector(tls_connector);
            ldap_conn_settings = ldap_conn_settings.set_starttls(settings.use_starttls);
        }

        match LdapConn::from_url_with_settings(ldap_conn_settings, url) {
            Ok(ldap_conn) => Ok(VkLdapContext {
                ldap_conn,
                settings,
            }),
            Err(err) => Err(VkLdapError::LdapCreateContextError(err)),
        }
    }

    fn bind(&mut self, user_dn: &str, password: &str) -> Result<()> {
        handle_ldap_error!(
            self.ldap_conn.simple_bind(user_dn, password),
            VkLdapError::LdapBindError
        );
        Ok(())
    }

    fn search(&mut self, username: &str) -> Result<String> {
        if let Some(bind_dn) = &self.settings.search_bind_dn {
            if let Some(bind_passwd) = &self.settings.search_bind_passwd {
                handle_ldap_error!(
                    self.ldap_conn.simple_bind(&bind_dn, &bind_passwd),
                    VkLdapError::LdapAdminBindError
                );
            }
        }

        let mut base = "";
        if let Some(sbase) = &self.settings.search_base {
            base = &sbase;
        }

        let mut filter = "objectClass=*";
        if let Some(sfilter) = &self.settings.search_filter {
            filter = &sfilter;
        }

        let mut attribute = "uid";
        if let Some(sattribute) = &self.settings.search_attribute {
            attribute = &sattribute;
        }

        let search_filter = format!("(&({filter})({attribute}={username}))");

        let (rs, _res) = handle_ldap_error!(
            self.ldap_conn.search(
                base,
                self.settings.search_scope,
                search_filter.as_str(),
                vec![&self.settings.search_dn_attribute],
            ),
            VkLdapError::LdapSearchError
        );

        if rs.len() == 0 {
            return Err(VkLdapError::NoLdapEntryFound(search_filter));
        }

        if rs.len() > 1 {
            return Err(VkLdapError::MultipleEntryFound(search_filter));
        }

        let entry = rs
            .into_iter()
            .next()
            .expect("there should be one element in rs");
        let sentry = SearchEntry::construct(entry);

        Ok(sentry.attrs[&self.settings.search_dn_attribute][0].clone())
    }
}

impl Drop for VkLdapContext {
    fn drop(&mut self) {
        match self.ldap_conn.unbind() {
            Ok(_) => (),
            Err(_) => (),
        }
    }
}

fn get_ldap_context(settings: VkLdapSettings) -> Result<VkLdapContext> {
    let config = LDAP_CONFIG.lock().unwrap();
    let url_opt = config.find_server();
    match url_opt {
        Some(url) => VkLdapContext::new(settings, url),
        None => Err(VkLdapError::NoServerConfigured),
    }
}

pub fn vk_ldap_bind(settings: VkLdapSettings, username: &str, password: &str) -> Result<()> {
    let mut ldap_ctx = get_ldap_context(settings)?;
    let prefix = &ldap_ctx.settings.bind_db_prefix;
    let suffix = &ldap_ctx.settings.bind_db_suffix;
    let user_dn = format!("{prefix}{username}{suffix}");
    ldap_ctx.bind(user_dn.as_str(), password)
}

pub fn vk_ldap_search_and_bind(
    settings: VkLdapSettings,
    username: &str,
    password: &str,
) -> Result<()> {
    let mut ldap_ctx = get_ldap_context(settings)?;
    let user_dn = ldap_ctx.search(username)?;
    ldap_ctx.bind(user_dn.as_str(), password)
}
