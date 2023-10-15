use std::{
    borrow::Cow,
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::smtp::session::TestSession;
use ahash::AHashMap;
use directory::config::ConfigDirectory;
use mail_auth::{dmarc::Policy, DkimResult, DmarcResult, IprevResult, SpfResult, MX};
use sieve::runtime::Variable;
use smtp::{
    config::{scripts::ConfigSieve, ConfigContext, IfBlock},
    core::{Session, SessionAddress, SMTP},
    inbound::AuthResult,
    scripts::{
        functions::html::{get_attribute, html_attr_tokens, html_img_area, html_to_tokens},
        ScriptResult,
    },
};
use tokio::runtime::Handle;
use utils::config::Config;

use crate::smtp::{TestConfig, TestSMTP};

const CONFIG: &str = r#"
[sieve]
from-name = "Sieve Daemon"
from-addr = "sieve@foobar.org"
return-path = ""
hostname = "mx.foobar.org"
no-capability-check = true

[sieve.limits]
redirects = 3
out-messages = 5
received-headers = 50
cpu = 10000
nested-includes = 5
duplicate-expiry = "7d"

[directory."spamdb"]
type = "sql"
address = "sqlite://%PATH%/test_antispam.db?mode=rwc"

[directory."spamdb".pool]
max-connections = 10
min-connections = 0
idle-timeout = "5m"

[directory."spamdb".lookup]
bayes-train = "INSERT INTO bayes_weights (h1, h2, ws, wh) VALUES (?, ?, ?, ?) ON CONFLICT(h1, h2) DO UPDATE SET ws = ws + excluded.ws, wh = wh + excluded.wh"
bayes-classify = "SELECT ws, wh FROM bayes_weights WHERE h1 = ? AND h2 = ?"
id-insert = "INSERT INTO id_timestamps (id, timestamp) VALUES (?, CURRENT_TIMESTAMP)"
id-lookup = "SELECT 1 FROM id_timestamps WHERE id = ?"
id-cleanup = "DELETE FROM id_timestamps WHERE (strftime('%s', 'now') - strftime('%s', timestamp)) < ?"

[directory."spam"]
type = "memory"

[directory."spam".lookup."free-domains"]
type = "glob"
comment = '#'
values = ["gmail.com", "googlemail.com", "yahoomail.com", "*.freemail.org"]

[directory."spam".lookup."disposable-domains"]
type = "glob"
comment = '#'
values = ["guerrillamail.com", "*.disposable.org"]

[directory."spam".lookup."redirectors"]
type = "glob"
comment = '#'
values = ["bit.ly", "redirect.io", "redirect.me", "redirect.org",
 "redirect.com", "redirect.net", "t.ly", "tinyurl.com"]

[directory."spam".lookup."dmarc-allow"]
type = "glob"
comment = '#'
values = ["dmarc-allow.org"]

[directory."spam".lookup."spf-dkim-allow"]
type = "glob"
comment = '#'
values = ["spf-dkim-allow.org"]

[directory."spam".lookup."mime-types"]
type = "map"
comment = '#'
values = ["html text/html|BAD", 
          "pdf application/pdf|NZ", 
          "txt text/plain|message/disposition-notification|text/rfc822-headers", 
          "zip AR", 
          "js BAD|NZ", 
          "hta BAD|NZ"]

[directory."spam".lookup."phishing-open"]
type = "glob"
comment = '#'
values = ["*://phishing-open.org", "*://phishing-open.com"]

[directory."spam".lookup."phishing-tank"]
type = "glob"
comment = '#'
values = ["*://phishing-tank.com", "*://phishing-tank.org"]

[directory."spam".lookup."trap-address"]
type = "glob"
comment = '#'
values = ["spamtrap@*"]

[directory."spam".lookup."options"]
type = "list"
values = ["AUTOLEARN_REPLIES"]

[resolver]
public-suffix = "file://%LIST_PATH%/public-suffix.dat"

[bayes]
min-learns = 10

[sieve.scripts]
"#;

const CREATE_TABLES: &[&str; 2] = &[
    "CREATE TABLE IF NOT EXISTS bayes_weights (
h1 INTEGER NOT NULL,
h2 INTEGER NOT NULL,
ws INTEGER,
wh INTEGER,
PRIMARY KEY (h1, h2)
)",
    "CREATE TABLE IF NOT EXISTS id_timestamps (
    id STRING PRIMARY KEY,
    timestamp DATETIME NOT NULL
)",
];

#[tokio::test(flavor = "multi_thread")]
async fn antispam() {
    /*tracing::subscriber::set_global_default(
        tracing_subscriber::FmtSubscriber::builder()
            .with_max_level(tracing::Level::TRACE)
            .finish(),
    )
    .unwrap();*/

    // Prepare config
    let tests = [
        "html",
        "subject",
        "bounce",
        "received",
        "messageid",
        "date",
        "from",
        "replyto",
        "recipient",
        "mime",
        "headers",
        "url",
        "dmarc",
        "ip",
        "helo",
        "rbl",
        "replies_out",
        "replies_in",
        "spamtrap",
        "bayes_classify",
    ];
    let mut core = SMTP::test();
    let qr = core.init_test_queue("smtp_antispam_test");
    let mut config = CONFIG
        .replace("%PATH%", qr._temp_dir.temp_dir.as_path().to_str().unwrap())
        .replace(
            "%LIST_PATH%",
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("resources")
                .join("smtp")
                .join("lists")
                .to_str()
                .unwrap(),
        );
    let base_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
        .join("resources")
        .join("config")
        .join("sieve");
    let prelude = fs::read_to_string(base_path.join("prelude.sieve")).unwrap();
    for test_name in tests {
        let script = fs::read_to_string(base_path.join(format!("{test_name}.sieve"))).unwrap();
        config.push_str(&format!("{test_name} = '''{prelude}\n{script}\n'''\n"));
    }

    // Parse config
    let config = Config::parse(&config).unwrap();
    let mut ctx = ConfigContext::new(&[]);
    ctx.directory = config.parse_directory().unwrap();
    core.sieve = config.parse_sieve(&mut ctx).unwrap();
    let config = &mut core.session.config;
    config.rcpt.relay = IfBlock::new(true);

    // Create tables
    let sdb = ctx.directory.directories.get("spamdb").unwrap();
    for query in CREATE_TABLES {
        sdb.query(query, &[]).await.unwrap();
    }

    // Add mock DNS entries
    for (domain, ip) in [
        ("bank.com", "127.0.0.1"),
        ("apple.com", "127.0.0.1"),
        ("youtube.com", "127.0.0.1"),
        ("twitter.com", "127.0.0.3"),
        ("dkimtrusted.org.dwl.dnswl.org", "127.0.0.3"),
        ("sh-malware.com.dbl.spamhaus.org", "127.0.0.5"),
        ("surbl-abuse.com.multi.surbl.org", "127.0.0.64"),
        ("uribl-grey.com.multi.uribl.com", "127.0.0.4"),
        ("sem-uribl.com.uribl.spameatingmonkey.net", "127.0.0.2"),
        ("sem-fresh15.com.fresh15.spameatingmonkey.net", "127.0.0.2"),
        (
            "b4a64d60f67529b0b18df66ea2f292e09e43c975.ebl.msbl.org",
            "127.0.0.2",
        ),
        (
            "a95bd658068a8315dc1864d6bb79632f47692621.ebl.msbl.org",
            "127.0.1.3",
        ),
        (
            "94c57fe69a113e875f772bdea55bf2c3.hashbl.surbl.org",
            "127.0.0.16",
        ),
        (
            "64aca53deb83db2ba30a59604ada2d80.hashbl.surbl.org",
            "127.0.0.64",
        ),
        (
            "02159eed92622b2fb8c83c659f269007.hashbl.surbl.org",
            "127.0.0.8",
        ),
    ] {
        core.resolvers.dns.ipv4_add(
            domain,
            vec![ip.parse().unwrap()],
            Instant::now() + Duration::from_secs(100),
        );
    }
    for mx in [
        "domain.org",
        "domain.co.uk",
        "gmail.com",
        "custom.disposable.org",
    ] {
        core.resolvers.dns.mx_add(
            mx,
            vec![MX {
                exchanges: vec!["127.0.0.1".parse().unwrap()],
                preference: 10,
            }],
            Instant::now() + Duration::from_secs(100),
        );
    }

    let core = Arc::new(core);

    // Run tests
    let base_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("resources")
        .join("smtp")
        .join("antispam");
    let span = tracing::info_span!("sieve_antispam");
    for test_name in tests {
        println!("===== {test_name} =====");
        let script = ctx.scripts.remove(test_name).unwrap();

        let contents = fs::read_to_string(base_path.join(format!("{test_name}.test"))).unwrap();
        let mut lines = contents.lines();
        let mut has_more = true;

        while has_more {
            let mut message = String::new();
            let mut in_params = true;
            let mut variables: HashMap<String, Variable> = HashMap::new();
            let mut expected_variables = AHashMap::new();

            // Build session
            let mut session = Session::test(core.clone());
            for line in lines.by_ref() {
                if in_params {
                    if line.is_empty() {
                        in_params = false;
                        continue;
                    }
                    let (param, value) = line.split_once(' ').unwrap();
                    let value = value.trim();
                    match param {
                        "remote_ip" => {
                            session.data.remote_ip = value.parse().unwrap();
                        }
                        "helo_domain" => {
                            session.data.helo_domain = value.to_string();
                        }
                        "authenticated_as" => {
                            session.data.authenticated_as = value.to_string();
                        }
                        "spf.result" | "spf_ehlo.result" => {
                            variables.insert(
                                param.to_string(),
                                SpfResult::from_str(value).as_str().to_string().into(),
                            );
                        }
                        "iprev.result" => {
                            variables.insert(
                                param.to_string(),
                                IprevResult::from_str(value).as_str().to_string().into(),
                            );
                        }
                        "dkim.result" | "arc.result" => {
                            variables.insert(
                                param.to_string(),
                                DkimResult::from_str(value).as_str().to_string().into(),
                            );
                        }
                        "dkim.domains" => {
                            variables.insert(
                                param.to_string(),
                                value
                                    .split_ascii_whitespace()
                                    .map(|s| Variable::from(s.to_string()))
                                    .collect::<Vec<_>>()
                                    .into(),
                            );
                        }
                        "envelope_from" => {
                            session.data.mail_from = Some(SessionAddress::new(value.to_string()));
                        }
                        "envelope_to" => {
                            session
                                .data
                                .rcpt_to
                                .push(SessionAddress::new(value.to_string()));
                        }
                        "iprev.ptr" | "dmarc.from" => {
                            variables.insert(param.to_string(), value.to_string().into());
                        }
                        "dmarc.result" => {
                            variables.insert(
                                param.to_string(),
                                DmarcResult::from_str(value).as_str().to_string().into(),
                            );
                        }
                        "dmarc.policy" => {
                            variables.insert(
                                param.to_string(),
                                Policy::from_str(value).as_str().to_string().into(),
                            );
                        }
                        "expect" => {
                            expected_variables.extend(value.split_ascii_whitespace().map(|v| {
                                v.split_once('=')
                                    .map(|(k, v)| {
                                        (
                                            k.to_lowercase(),
                                            if v.contains('.') {
                                                Variable::Float(v.parse().unwrap())
                                            } else {
                                                Variable::Integer(v.parse().unwrap())
                                            },
                                        )
                                    })
                                    .unwrap_or((v.to_lowercase(), Variable::Integer(1)))
                            }));
                        }
                        _ if param.starts_with("param.") | param.starts_with("tls.") => {
                            variables.insert(param.to_string(), value.to_string().into());
                        }
                        _ => panic!("Invalid parameter {param:?}"),
                    }
                } else {
                    has_more = line.trim().eq_ignore_ascii_case("<!-- NEXT TEST -->");
                    if !has_more {
                        message.push_str(line);
                        message.push_str("\r\n");
                    } else {
                        break;
                    }
                }
            }

            if message.is_empty() {
                panic!("No message found");
            }

            // Build script params
            let mut expected = expected_variables.keys().collect::<Vec<_>>();
            expected.sort_unstable_by(|a, b| b.cmp(a));
            println!("Testing tags {:?}", expected);
            let mut params = session
                .build_script_parameters("data")
                .with_expected_variables(expected_variables)
                .with_message(Arc::new(message.into_bytes()));
            for (name, value) in variables {
                params = params.set_variable(name, value);
            }

            // Run script
            let handle = Handle::current();
            let span = span.clone();
            let core_ = core.clone();
            let script = script.clone();
            match core
                .spawn_worker(move || core_.run_script_blocking(script, params, handle, span))
                .await
                .unwrap()
            {
                ScriptResult::Accept { .. } => {}
                ScriptResult::Reject(message) => panic!("{}", message),
                ScriptResult::Replace {
                    message,
                    modifications,
                } => println!(
                    "Replace: {} with modifications {:?}",
                    String::from_utf8_lossy(&message),
                    modifications
                ),
                ScriptResult::Discard => println!("Discard"),
            }
        }
    }
}

#[test]
fn html_tokens() {
    for (input, expected) in [
        (
            "<html>hello<br/>world<br/></html>",
            vec![
                Variable::from("<html".to_string()),
                Variable::from("_hello".to_string()),
                Variable::from("<br/".to_string()),
                Variable::from("_world".to_string()),
                Variable::from("<br/".to_string()),
                Variable::from("</html".to_string()),
            ],
        ),
        (
            "<html>using &lt;><br/></html>",
            vec![
                Variable::from("<html".to_string()),
                Variable::from("_using <>".to_string()),
                Variable::from("<br/".to_string()),
                Variable::from("</html".to_string()),
            ],
        ),
        (
            "test <not br/>tag<br />",
            vec![
                Variable::from("_test".to_string()),
                Variable::from("<not br/".to_string()),
                Variable::from("_ tag".to_string()),
                Variable::from("<br /".to_string()),
            ],
        ),
        (
            "<>< ><tag\n/>>hello    world< br \n />",
            vec![
                Variable::from("<".to_string()),
                Variable::from("<".to_string()),
                Variable::from("<tag /".to_string()),
                Variable::from("_>hello world".to_string()),
                Variable::from("<br /".to_string()),
            ],
        ),
        (
            concat!(
                "<head><title>ignore head</title><not head>xyz</not head></head>",
                "<h1>&lt;body&gt;</h1>"
            ),
            vec![
                Variable::from("<head".to_string()),
                Variable::from("<title".to_string()),
                Variable::from("_ignore head".to_string()),
                Variable::from("</title".to_string()),
                Variable::from("<not head".to_string()),
                Variable::from("_xyz".to_string()),
                Variable::from("</not head".to_string()),
                Variable::from("</head".to_string()),
                Variable::from("<h1".to_string()),
                Variable::from("_<body>".to_string()),
                Variable::from("</h1".to_string()),
            ],
        ),
        (
            concat!(
                "<p>what is &heartsuit;?</p><p>&#x000DF;&Abreve;&#914;&gamma; ",
                "don&apos;t hurt me.</p>"
            ),
            vec![
                Variable::from("<p".to_string()),
                Variable::from("_what is ♥?".to_string()),
                Variable::from("</p".to_string()),
                Variable::from("<p".to_string()),
                Variable::from("_ßĂΒγ don't hurt me.".to_string()),
                Variable::from("</p".to_string()),
            ],
        ),
        (
            concat!(
                "<!--[if mso]><style type=\"text/css\">body, table, td, a, p, ",
                "span, ul, li {font-family: Arial, sans-serif!important;}</style><![endif]-->",
                "this is <!-- <> < < < < ignore  > -> here -->the actual<!--> text"
            ),
            vec![
                Variable::from(
                    concat!(
                        "<!--[if mso]><style type=\"text/css\">body, table, ",
                        "td, a, p, span, ul, li {font-family: Arial, sans-serif!",
                        "important;}</style><![endif]--"
                    )
                    .to_string(),
                ),
                Variable::from("_this is".to_string()),
                Variable::from("<!-- <> < < < < ignore  > -> here --".to_string()),
                Variable::from("_ the actual".to_string()),
                Variable::from("<!--".to_string()),
                Variable::from("_ text".to_string()),
            ],
        ),
        (
            "   < p >  hello < / p > < p > world < / p >   !!! < br > ",
            vec![
                Variable::from("<p ".to_string()),
                Variable::from("_hello".to_string()),
                Variable::from("</p ".to_string()),
                Variable::from("<p ".to_string()),
                Variable::from("_ world".to_string()),
                Variable::from("</p ".to_string()),
                Variable::from("_ !!!".to_string()),
                Variable::from("<br ".to_string()),
            ],
        ),
        (
            " <p>please unsubscribe <a href=#>here</a>.</p> ",
            vec![
                Variable::from("<p".to_string()),
                Variable::from("_please unsubscribe".to_string()),
                Variable::from("<a href=#".to_string()),
                Variable::from("_ here".to_string()),
                Variable::from("</a".to_string()),
                Variable::from("_.".to_string()),
                Variable::from("</p".to_string()),
            ],
        ),
    ] {
        assert_eq!(html_to_tokens(input), expected, "Failed for '{:?}'", input);
    }

    for (input, expected) in [
        (
            concat!(
                "<a href=\"a\">text</a>",
                "<a href =\"b\">text</a>",
                "<a href= \"c\">text</a>",
                "<a href = \"d\">text</a>",
                "<  a href = \"e\" >text</a>",
                "<a hrefer = \"ignore\" >text</a>",
                "< anchor href = \"x\">text</a>",
            ),
            vec![
                Variable::from("a".to_string()),
                Variable::from("b".to_string()),
                Variable::from("c".to_string()),
                Variable::from("d".to_string()),
                Variable::from("e".to_string()),
            ],
        ),
        (
            concat!(
                "<a href=a>text</a>",
                "<a href =b>text</a>",
                "<a href= c>text</a>",
                "<a href = d>text</a>",
                "< a href  =  e >text</a>",
                "<a hrefer = ignore>text</a>",
                "<anchor href=x>text</a>",
            ),
            vec![
                Variable::from("a".to_string()),
                Variable::from("b".to_string()),
                Variable::from("c".to_string()),
                Variable::from("d".to_string()),
                Variable::from("e".to_string()),
            ],
        ),
        (
            concat!(
                "<!-- <a href=a>text</a>",
                "<a href =b>text</a>",
                "<a href= c>--text</a>-->",
                "<a href = \"hello world\">text</a>",
                "< a href  =  test ignore>text</a>",
                "< a href  =  fudge href ignore>text</a>",
                "<a href=foobar> a href = \"unknown\" </a>",
            ),
            vec![
                Variable::from("hello world".to_string()),
                Variable::from("test".to_string()),
                Variable::from("fudge".to_string()),
                Variable::from("foobar".to_string()),
            ],
        ),
    ] {
        assert_eq!(
            html_attr_tokens(input, "a", vec![Cow::from("href")]),
            expected,
            "Failed for '{:?}'",
            input
        );
    }

    for (tag, attr_name, expected) in [
        ("<img width=200 height=400", "width", "200"),
        ("<img width=200 height=400", "height", "400"),
        ("<img width = 200 height = 400", "width", "200"),
        ("<img width = 200 height = 400", "height", "400"),
        ("<img width =200 height =400", "width", "200"),
        ("<img width =200 height =400", "height", "400"),
        ("<img width= 200 height= 400", "width", "200"),
        ("<img width= 200 height= 400", "height", "400"),
        ("<img width=\"200\" height=\"400\"", "width", "200"),
        ("<img width=\"200\" height=\"400\"", "height", "400"),
        ("<img width = \"200\" height = \"400\"", "width", "200"),
        ("<img width = \"200\" height = \"400\"", "height", "400"),
        (
            "<img width=\" 200 % \" height=\" 400 % \"",
            "width",
            " 200 % ",
        ),
        (
            "<img width=\" 200 % \" height=\" 400 % \"",
            "height",
            " 400 % ",
        ),
    ] {
        assert_eq!(
            get_attribute(tag, attr_name).unwrap_or_default(),
            expected,
            "failed for {tag:?}, {attr_name:?}"
        );
    }

    assert_eq!(
        html_img_area(&html_to_tokens(concat!(
            "<img width=200 height=400 />",
            "20",
            "30",
            "<img width=10% height=\" 20% \"/>",
            "<img width=\"50\" height   =   \"60\">"
        ))),
        92600
    );
}

trait ParseConfigValue: Sized {
    fn from_str(value: &str) -> Self;
}

impl ParseConfigValue for SpfResult {
    fn from_str(value: &str) -> Self {
        match value {
            "pass" => SpfResult::Pass,
            "fail" => SpfResult::Fail,
            "softfail" => SpfResult::SoftFail,
            "neutral" => SpfResult::Neutral,
            "none" => SpfResult::None,
            "temperror" => SpfResult::TempError,
            "permerror" => SpfResult::PermError,
            _ => panic!("Invalid SPF result"),
        }
    }
}

impl ParseConfigValue for IprevResult {
    fn from_str(value: &str) -> Self {
        match value {
            "pass" => IprevResult::Pass,
            "fail" => IprevResult::Fail(mail_auth::Error::NotAligned),
            "temperror" => IprevResult::TempError(mail_auth::Error::NotAligned),
            "permerror" => IprevResult::PermError(mail_auth::Error::NotAligned),
            "none" => IprevResult::None,
            _ => panic!("Invalid IPREV result"),
        }
    }
}

impl ParseConfigValue for DkimResult {
    fn from_str(value: &str) -> Self {
        match value {
            "pass" => DkimResult::Pass,
            "none" => DkimResult::None,
            "neutral" => DkimResult::Neutral(mail_auth::Error::NotAligned),
            "fail" => DkimResult::Fail(mail_auth::Error::NotAligned),
            "permerror" => DkimResult::PermError(mail_auth::Error::NotAligned),
            "temperror" => DkimResult::TempError(mail_auth::Error::NotAligned),
            _ => panic!("Invalid DKIM result"),
        }
    }
}

impl ParseConfigValue for DmarcResult {
    fn from_str(value: &str) -> Self {
        match value {
            "pass" => DmarcResult::Pass,
            "fail" => DmarcResult::Fail(mail_auth::Error::NotAligned),
            "temperror" => DmarcResult::TempError(mail_auth::Error::NotAligned),
            "permerror" => DmarcResult::PermError(mail_auth::Error::NotAligned),
            "none" => DmarcResult::None,
            _ => panic!("Invalid DMARC result"),
        }
    }
}

impl ParseConfigValue for Policy {
    fn from_str(value: &str) -> Self {
        match value {
            "reject" => Policy::Reject,
            "quarantine" => Policy::Quarantine,
            "none" => Policy::None,
            _ => panic!("Invalid DMARC policy"),
        }
    }
}
