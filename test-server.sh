#!/usr/bin/env bash
set -e

echo "Starting GreenMail IMAP test server..."
podman rm -f imap-test 2>/dev/null || true
podman run -d --name imap-test \
  -p 3025:3025 -p 3143:3143 -p 3993:3993 \
  -e GREENMAIL_OPTS="-Dgreenmail.setup.test.all -Dgreenmail.hostname=0.0.0.0 -Dgreenmail.users=test:password@localhost -Dgreenmail.verbose" \
  greenmail/standalone:2.1.2

sleep 3

# Create folders + send test emails
python3 -c "
import imaplib, ssl, smtplib
from email.mime.text import MIMEText
from email.utils import make_msgid

ctx = ssl.create_default_context()
ctx.check_hostname = False
ctx.verify_mode = ssl.CERT_NONE

m = imaplib.IMAP4_SSL('127.0.0.1', 3993, ssl_context=ctx)
m.login('test', 'password')
m.create('Drafts')
m.create('Sent')
m.create('Trash')
m.logout()

msg1 = MIMEText('Hello, this is test email body for testing the IMAP MCP server.')
msg1['Subject'] = 'Project Update Q2'
msg1['From'] = 'alice@example.com'
msg1['To'] = 'test@localhost'
msg1['Message-ID'] = make_msgid(domain='example.com')

with smtplib.SMTP('127.0.0.1', 3025) as s:
    s.send_message(msg1)

msg2 = MIMEText('Thanks Alice, I reviewed the Q2 numbers. Looks good!')
msg2['Subject'] = 'Re: Project Update Q2'
msg2['From'] = 'bob@example.com'
msg2['To'] = 'test@localhost'
msg2['In-Reply-To'] = msg1['Message-ID']
msg2['References'] = msg1['Message-ID']

with smtplib.SMTP('127.0.0.1', 3025) as s:
    s.send_message(msg2)

msg3 = MIMEText('Team meeting tomorrow at 10am in room 4B. Please confirm attendance.')
msg3['Subject'] = 'Team Meeting Tomorrow'
msg3['From'] = 'boss@company.com'
msg3['To'] = 'test@localhost'

with smtplib.SMTP('127.0.0.1', 3025) as s:
    s.send_message(msg3)
"

echo ""
echo "GreenMail ready:"
echo "  IMAPS: 127.0.0.1:3993 (user: test, pass: password)"
echo "  SMTP:  127.0.0.1:3025"
echo "  Folders: INBOX (3 emails), Drafts, Sent, Trash"
echo ""
echo "Stop with: podman rm -f imap-test"
