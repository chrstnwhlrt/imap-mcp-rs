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
import imaplib, ssl, smtplib, time
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

# Sleep 1.1s between sends so the auto-generated Date: headers (which are
# second-granular) differ — the integration tests rely on chronological
# order to assert 'alice's original comes before bob's reply'.
time.sleep(1.1)

msg2 = MIMEText('Thanks Alice, I reviewed the Q2 numbers. Looks good!')
msg2['Subject'] = 'Re: Project Update Q2'
msg2['From'] = 'bob@example.com'
msg2['To'] = 'test@localhost'
msg2['In-Reply-To'] = msg1['Message-ID']
msg2['References'] = msg1['Message-ID']

with smtplib.SMTP('127.0.0.1', 3025) as s:
    s.send_message(msg2)

time.sleep(1.1)

msg3 = MIMEText('Team meeting tomorrow at 10am in room 4B. Please confirm attendance.')
msg3['Subject'] = 'Team Meeting Tomorrow'
msg3['From'] = 'boss@company.com'
msg3['To'] = 'test@localhost'

with smtplib.SMTP('127.0.0.1', 3025) as s:
    s.send_message(msg3)

time.sleep(1.1)

# Subject collision: same kernel as msg1 but no References / In-Reply-To.
# Classic false-positive case for subject-based thread matching — a later
# 'Project Update Q2' mail (e.g. next quarter's reminder) that strict=true
# must NOT pull into the original Q2 thread, but strict=false should.
msg4 = MIMEText('Quick reminder about the Q2 update — different context.')
msg4['Subject'] = 'Project Update Q2'
msg4['From'] = 'charlie@example.com'
msg4['To'] = 'test@localhost'
msg4['Message-ID'] = make_msgid(domain='example.com')

with smtplib.SMTP('127.0.0.1', 3025) as s:
    s.send_message(msg4)
"

echo ""
echo "GreenMail ready:"
echo "  IMAPS: 127.0.0.1:3993 (user: test, pass: password)"
echo "  SMTP:  127.0.0.1:3025"
echo "  Folders: INBOX (4 emails), Drafts, Sent, Trash"
echo ""
echo "Stop with: podman rm -f imap-test"
