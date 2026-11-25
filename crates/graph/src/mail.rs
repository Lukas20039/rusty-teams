//! Outlook mail through the Teams session. Reading and filing use Graph (`Mail.ReadWrite`); sending
//! uses Outlook REST v2.0, because only its token for the Teams web app carries `Mail.Send`.
//! Both APIs accept the same message ids.

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use session::{Method, Request, Scope};

use crate::client::Graph;
use crate::error::{Error, Result};
use crate::people::decode_binary;
use crate::urls::segment;

const MAIL_SCOPE: &str = "Mail.ReadWrite";
const OUTLOOK: &str = "https://outlook.office.com";
const SEND_SCOPE: &str = "Mail.Send";
const API: &str = "https://graph.microsoft.com/v1.0/me";
const OUTLOOK_API: &str = "https://outlook.office.com/api/v2.0/me";
const SUMMARY_FIELDS: &str = "id,subject,from,bodyPreview,receivedDateTime,isRead,hasAttachments";
/// ponytail: lists the newest 50 per folder; follow `@odata.nextLink` when older mail is needed.
pub const MAIL_PAGE: usize = 50;
pub const INBOX: &str = "inbox";
pub const ARCHIVE: &str = "archive";
pub const DELETED: &str = "deleteditems";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailFolder {
    pub id: String,
    pub name: String,
    pub unread: u64,
    pub total: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MailSummary {
    pub id: String,
    pub subject: String,
    pub from_name: String,
    pub from_address: String,
    pub preview: String,
    pub received: Option<DateTime<Utc>>,
    pub is_read: bool,
    pub has_attachments: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MailMessage {
    pub summary: MailSummary,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    /// Plain text: Graph converts HTML mail on request.
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailAttachment {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub inline: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Response {
    Reply,
    ReplyAll,
    Forward,
}

impl Response {
    fn action(self) -> &'static str {
        match self {
            Response::Reply => "reply",
            Response::ReplyAll => "replyall",
            Response::Forward => "forward",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OutgoingMail {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    /// HTML.
    pub body: String,
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

fn time(value: &Value) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

fn items(body: &Value) -> impl Iterator<Item = &Value> {
    body["value"].as_array().into_iter().flatten()
}

fn addresses(recipients: &Value) -> Vec<String> {
    recipients
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|recipient| recipient["emailAddress"]["address"].as_str())
        .map(str::to_owned)
        .collect()
}

/// Inbox first, the rest in the order Outlook lists them.
pub fn parse_folders(body: &Value, inbox_id: &str) -> Vec<MailFolder> {
    let mut folders: Vec<MailFolder> = items(body)
        .map(|folder| MailFolder {
            id: text(&folder["id"]),
            name: text(&folder["displayName"]),
            unread: folder["unreadItemCount"].as_u64().unwrap_or(0),
            total: folder["totalItemCount"].as_u64().unwrap_or(0),
        })
        .collect();
    folders.sort_by_key(|folder| folder.id != inbox_id);
    folders
}

pub fn parse_summary(message: &Value) -> MailSummary {
    let from = &message["from"]["emailAddress"];
    MailSummary {
        id: text(&message["id"]),
        subject: text(&message["subject"]),
        from_name: text(&from["name"]),
        from_address: text(&from["address"]),
        preview: text(&message["bodyPreview"]),
        received: time(&message["receivedDateTime"]),
        is_read: message["isRead"].as_bool().unwrap_or(true),
        has_attachments: message["hasAttachments"].as_bool().unwrap_or(false),
    }
}

pub fn parse_summaries(body: &Value) -> Vec<MailSummary> {
    items(body).map(parse_summary).collect()
}

pub fn parse_message(message: &Value) -> MailMessage {
    MailMessage {
        summary: parse_summary(message),
        to: addresses(&message["toRecipients"]),
        cc: addresses(&message["ccRecipients"]),
        body: text(&message["body"]["content"]),
    }
}

pub fn parse_attachments(body: &Value) -> Vec<MailAttachment> {
    items(body)
        .map(|attachment| MailAttachment {
            id: text(&attachment["id"]),
            name: text(&attachment["name"]),
            size: attachment["size"].as_u64().unwrap_or(0),
            inline: attachment["isInline"].as_bool().unwrap_or(false),
        })
        .collect()
}

fn recipients(addresses: &[String]) -> Value {
    addresses
        .iter()
        .map(|address| json!({"EmailAddress": {"Address": address}}))
        .collect()
}

pub fn send_body(mail: &OutgoingMail) -> Value {
    json!({
        "Message": {
            "Subject": mail.subject,
            "Body": {"ContentType": "HTML", "Content": mail.body},
            "ToRecipients": recipients(&mail.to),
            "CcRecipients": recipients(&mail.cc),
        },
        "SaveToSentItems": true,
    })
}

/// Outlook puts the HTML comment above the quoted mail.
pub fn response_body(response: Response, comment_html: &str, to: &[String]) -> Value {
    match response {
        Response::Forward => json!({"Comment": comment_html, "ToRecipients": recipients(to)}),
        Response::Reply | Response::ReplyAll => json!({"Comment": comment_html}),
    }
}

/// One mail of a conversation as its text body carries it: the newest first, then each quoted one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ThreadPart {
    /// Empty for the newest part.
    pub from: String,
    pub sent: String,
    pub body: String,
}

const FROM_KEYS: [&str; 2] = ["From:", "Von:"];
const SENT_KEYS: [&str; 4] = ["Sent:", "Gesendet:", "Date:", "Datum:"];
const HEADER_KEYS: [&str; 10] = [
    "To:",
    "An:",
    "Cc:",
    "Bcc:",
    "Subject:",
    "Betreff:",
    "From:",
    "Von:",
    "Sent:",
    "Gesendet:",
];

fn value_after<'a>(line: &'a str, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| line.strip_prefix(key))
        .map(str::trim)
}

fn is_separator(line: &str) -> bool {
    line.len() >= 8
        && line
            .chars()
            .all(|character| character == '_' || character == '-')
}

fn is_attribution(line: &str) -> bool {
    (line.starts_with("On ") && line.ends_with("wrote:"))
        || (line.starts_with("Am ") && line.ends_with(':') && line.contains(" schrieb"))
}

/// Blank runs become one empty line, so a converted HTML body does not stretch.
fn compact(lines: &[&str]) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in lines {
        let line = line.trim_end();
        if line.trim().is_empty() && out.last().is_none_or(|last| last.is_empty()) {
            continue;
        }
        out.push(if line.trim().is_empty() { "" } else { line });
    }
    while out
        .last()
        .is_some_and(|last| last.is_empty() || is_separator(last.trim()))
    {
        out.pop();
    }
    out.join("\n")
}

/// Splits a text body at the "From: … Sent: …" blocks and "On … wrote:" lines Outlook and
/// other clients put above each quoted mail.
pub fn split_thread(body: &str) -> Vec<ThreadPart> {
    let lines: Vec<&str> = body.lines().collect();
    let mut parts = Vec::new();
    let mut current = ThreadPart::default();
    let mut start = 0;
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index].trim();
        let header_block = value_after(line, &FROM_KEYS).is_some()
            && lines[index + 1..lines.len().min(index + 5)]
                .iter()
                .any(|next| value_after(next.trim(), &SENT_KEYS).is_some());
        let original = line.starts_with("-----") && line.ends_with("-----") && line.len() > 10;
        if !(header_block || original || is_attribution(line)) {
            index += 1;
            continue;
        }
        current.body = compact(&lines[start..index]);
        parts.push(std::mem::take(&mut current));
        if is_attribution(line) {
            current.from = line.to_owned();
            index += 1;
        } else {
            if original {
                index += 1;
            }
            while let Some(next) = lines.get(index).map(|next| next.trim())
                && (value_after(next, &HEADER_KEYS).is_some()
                    || value_after(next, &SENT_KEYS).is_some())
            {
                if let Some(from) = value_after(next, &FROM_KEYS) {
                    current.from = from.to_owned();
                } else if let Some(sent) = value_after(next, &SENT_KEYS) {
                    current.sent = sent.to_owned();
                }
                index += 1;
            }
        }
        start = index;
    }
    current.body = compact(&lines[start..]);
    parts.push(current);
    parts
}

/// Splits what the user typed into the To or Cc field.
pub fn split_addresses(field: &str) -> Vec<String> {
    field
        .split([',', ';', ' ', '\n'])
        .map(str::trim)
        .filter(|address| address.contains('@'))
        .map(str::to_owned)
        .collect()
}

impl Graph {
    fn mail_scope() -> Scope {
        Scope::graph(MAIL_SCOPE)
    }

    pub async fn mail_folders(&self) -> Result<Vec<MailFolder>> {
        let inbox = self
            .get(
                &format!("{API}/mailFolders/{INBOX}?$select=id"),
                &Self::mail_scope(),
            )
            .await?;
        let folders = self
            .get(
                &format!(
                    "{API}/mailFolders?$top=100&$select=id,displayName,unreadItemCount,totalItemCount"
                ),
                &Self::mail_scope(),
            )
            .await?;
        Ok(parse_folders(
            &folders,
            inbox["id"].as_str().unwrap_or(INBOX),
        ))
    }

    pub async fn folder_mail(&self, folder_id: &str) -> Result<Vec<MailSummary>> {
        let url = format!(
            "{API}/mailFolders/{}/messages?$top={MAIL_PAGE}&$select={SUMMARY_FIELDS}&$orderby=receivedDateTime%20desc",
            segment(folder_id)
        );
        Ok(parse_summaries(&self.get(&url, &Self::mail_scope()).await?))
    }

    pub async fn search_mail(&self, query: &str) -> Result<Vec<MailSummary>> {
        let phrase = format!("\"{}\"", query.replace('"', ""));
        let url = format!(
            "{API}/messages?$search={}&$top={MAIL_PAGE}&$select={SUMMARY_FIELDS}",
            segment(&phrase)
        );
        Ok(parse_summaries(&self.get(&url, &Self::mail_scope()).await?))
    }

    pub async fn mail_message(&self, message_id: &str) -> Result<MailMessage> {
        let mut request = Request::get(format!(
            "{API}/messages/{}?$select={SUMMARY_FIELDS},toRecipients,ccRecipients,body",
            segment(message_id)
        ));
        request.headers.push((
            "Prefer".to_owned(),
            "outlook.body-content-type=\"text\"".to_owned(),
        ));
        let answer = self.session().send(request, &Self::mail_scope()).await?;
        Ok(parse_message(&answer.body))
    }

    pub async fn mail_attachments(&self, message_id: &str) -> Result<Vec<MailAttachment>> {
        let url = format!(
            "{API}/messages/{}/attachments?$select=id,name,size,isInline",
            segment(message_id)
        );
        Ok(parse_attachments(
            &self.get(&url, &Self::mail_scope()).await?,
        ))
    }

    pub async fn mail_attachment_bytes(
        &self,
        message_id: &str,
        attachment_id: &str,
    ) -> Result<Vec<u8>> {
        let url = format!(
            "{API}/messages/{}/attachments/{}/$value",
            segment(message_id),
            segment(attachment_id)
        );
        let request = Request::binary_get(&url);
        let answer = self
            .session()
            .batch(std::slice::from_ref(&request), &Self::mail_scope())
            .await?
            .remove(0);
        Ok(decode_binary(&url, answer)?.bytes)
    }

    pub async fn set_mail_read(&self, message_id: &str, read: bool) -> Result<()> {
        let url = format!("{API}/messages/{}", segment(message_id));
        self.session()
            .request(
                Method::Patch,
                &url,
                &Self::mail_scope(),
                Some(json!({"isRead": read})),
            )
            .await?;
        Ok(())
    }

    /// Moves a message to a folder id or a well-known name such as `archive` or `deleteditems`.
    pub async fn move_mail(&self, message_id: &str, destination: &str) -> Result<()> {
        let url = format!("{API}/messages/{}/move", segment(message_id));
        self.session()
            .request(
                Method::Post,
                &url,
                &Self::mail_scope(),
                Some(json!({"destinationId": destination})),
            )
            .await?;
        Ok(())
    }

    pub async fn send_mail(&self, mail: &OutgoingMail) -> Result<()> {
        if mail.to.is_empty() {
            return Err(Error::Upload("a mail needs a recipient".into()));
        }
        self.outlook_post(&format!("{OUTLOOK_API}/sendmail"), send_body(mail))
            .await
    }

    pub async fn respond_to_mail(
        &self,
        message_id: &str,
        response: Response,
        comment: &str,
        to: &[String],
    ) -> Result<()> {
        if response == Response::Forward && to.is_empty() {
            return Err(Error::Upload("a forward needs a recipient".into()));
        }
        let url = format!(
            "{OUTLOOK_API}/messages/{}/{}",
            segment(message_id),
            response.action()
        );
        self.outlook_post(&url, response_body(response, comment, to))
            .await
    }

    async fn outlook_post(&self, url: &str, body: Value) -> Result<()> {
        self.session()
            .send(
                Request::with_body(Method::Post, url, body),
                &Scope::new(OUTLOOK, SEND_SCOPE),
            )
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_inbox_comes_first_and_counts_parse() {
        let body = json!({"value": [
            {"id": "a", "displayName": "Archiv", "unreadItemCount": 0, "totalItemCount": 9},
            {"id": "in", "displayName": "Posteingang", "unreadItemCount": 5, "totalItemCount": 1414},
        ]});
        let folders = parse_folders(&body, "in");
        assert_eq!(folders[0].name, "Posteingang");
        assert_eq!((folders[0].unread, folders[0].total), (5, 1414));
        assert_eq!(folders[1].id, "a");
    }

    #[test]
    fn a_message_parses_sender_recipients_and_text_body() {
        let message = parse_message(&json!({
            "id": "m1",
            "subject": "Plan",
            "from": {"emailAddress": {"name": "Mara", "address": "mara@example.com"}},
            "bodyPreview": "Hi",
            "receivedDateTime": "2026-11-25T08:00:00Z",
            "isRead": false,
            "hasAttachments": true,
            "toRecipients": [{"emailAddress": {"address": "me@example.com"}}],
            "ccRecipients": [],
            "body": {"contentType": "text", "content": "Hi\nthere"}
        }));
        assert_eq!(message.summary.from_name, "Mara");
        assert!(!message.summary.is_read && message.summary.has_attachments);
        assert!(message.summary.received.is_some());
        assert_eq!(message.to, ["me@example.com"]);
        assert_eq!(message.body, "Hi\nthere");
    }

    #[test]
    fn a_new_mail_goes_out_as_html_and_is_kept_in_sent_items() {
        let body = send_body(&OutgoingMail {
            to: vec!["a@example.com".into()],
            cc: Vec::new(),
            subject: "S".into(),
            body: "B".into(),
        });
        assert_eq!(body["Message"]["Body"]["ContentType"], "HTML");
        assert_eq!(
            body["Message"]["ToRecipients"][0]["EmailAddress"]["Address"],
            "a@example.com"
        );
        assert_eq!(body["SaveToSentItems"], true);
    }

    #[test]
    fn reply_comments_carry_the_html_as_is() {
        let body = response_body(Response::Reply, "<b>ok</b>", &[]);
        assert_eq!(body["Comment"], "<b>ok</b>");
        assert!(body.get("ToRecipients").is_none());
        let forward = response_body(Response::Forward, "fyi", &["x@example.com".to_owned()]);
        assert_eq!(
            forward["ToRecipients"][0]["EmailAddress"]["Address"],
            "x@example.com"
        );
    }

    #[test]
    fn a_thread_splits_at_outlook_headers_and_attribution_lines() {
        let body = "Thanks!\n\n\n\nBest\n________________________________\nFrom: Mara <m@example.com>\nSent: Monday\nTo: Me\nSubject: Plan\n\nLooks good.\n\nVon: Jonas\nGesendet: Sonntag\nBetreff: Plan\nErster Entwurf\nOn Fri, Priya wrote:\n> hi";
        let parts = split_thread(body);
        assert_eq!(parts.len(), 4);
        assert_eq!(
            parts[0],
            ThreadPart {
                from: String::new(),
                sent: String::new(),
                body: "Thanks!\n\nBest".into()
            }
        );
        assert_eq!(
            (parts[1].from.as_str(), parts[1].sent.as_str()),
            ("Mara <m@example.com>", "Monday")
        );
        assert_eq!(parts[1].body, "Looks good.");
        assert_eq!(
            (parts[2].from.as_str(), parts[2].body.as_str()),
            ("Jonas", "Erster Entwurf")
        );
        assert_eq!(
            (parts[3].from.as_str(), parts[3].body.as_str()),
            ("On Fri, Priya wrote:", "> hi")
        );
    }

    #[test]
    fn a_from_line_in_the_text_is_not_a_quote() {
        let parts = split_thread("From: the start\nwe planned it");
        assert_eq!(parts.len(), 1);
    }

    #[test]
    fn typed_recipients_split_on_commas_semicolons_and_spaces() {
        assert_eq!(
            split_addresses("a@x.com; b@y.com, nope  c@z.com"),
            ["a@x.com", "b@y.com", "c@z.com"]
        );
    }
}
