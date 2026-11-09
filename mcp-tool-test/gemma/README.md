# Gemma-Test der IMAP-Werkzeuge

Misst, ob Gemma 4 E4B über den Sprachassistenten Otomo die Werkzeuge von imap-mcp-rs richtig nutzt: das richtige Werkzeug mit den richtigen Argumenten, den richtigen Endzustand im Postfach und eine ehrliche Antwort. Gemma bekommt dabei alles, was sie in Otomo hat: dessen Systemprompt, den Torwächter, die eigenen Werkzeuge (Erinnerungen, Agenten, Merkliste) und die Werkzeuge des Servers über Otomos MCP-Client. Es antwortet der echte Server gegen die Welt aus `welt.py`, einen Dovecot im Container mit einem erfundenen Postfach. Eine Anfrage an ein echtes Postfach geht dabei nie raus.

## Bestandteile

- `welt.py` und `dump.py` kommen von der Server-Seite: die Welt, ihre Konfigurationen in `run/` und der Dump.
- `kick.py` ist ein Regressionstest für den Server, nicht für Gemma. Er beendet vor jedem Aufruf die Sitzung per `doveadm kick`, wie es Gmail und Outlook nach einer Pause tun, und prüft, dass Lesen und Schreiben danach vollständig gelingen und nichts doppelt geschrieben wird.
- `run.py` fragt den Katalog über `otomo-probe chat --json` ab. Vor jedem Gespräch setzt es die Welt zurück (`welt.py reset`), danach liest `mailstate.py` das Postfach. Die Konfiguration von Otomo wird für den Lauf kopiert: nur eine Identität, nur der Mail-Server, ohne Rückfrage vor Aufrufen und mit dem Proxy vor Ollama. Die Kopie liegt nur während des Laufs vor, lesbar nur für den Besitzer und wird nie ausgegeben.
- `mailstate.py` liest den Endzustand nur lesend (EXAMINE, BODY.PEEK) über die Verbindung von `welt.py`: Ordner, Nachrichten mit Message-ID, Flags und Empfängern, dazu den Text eigener Entwürfe und gesendeter Mails.
- `textproxy.py` sitzt als MCP-Server zwischen Otomo und imap-mcp-rs. Er setzt die Texte einer Variante ein (`--texts`) und hält jeden Aufruf mit seinem Ergebnis fest. Unter `emulate` bildet er Serveränderungen nach, bevor der Server sie hat: die Suche mit mehreren Wörtern, Hinweis und neueste Mails bei leerer Suche, Ortszeit, den Ordnerfehler mit Liste, Ordner per Anzeigename oder Rolle und die Mail im Fokus. Seit `tool_set = "voice"` liefert der Server die gemessene Fassung selbst, für eine Messung des Servers braucht es also keine Texte.
- `proxy.py` vor Ollama hält jede Modellanfrage mit Gemmas Denken und der Tokenzahl fest. `why.py TAG REGEX` zeigt, warum sie ein Werkzeug nahm.
- `cases.py`: 33 Bitten (`CASES`), 14 zurückgehaltene (`HELD`) und zwei mit Bindestrich (`EXTRA`), jede mit ihrer Prüfung. Folgefragen laufen im selben Gespräch wie die Bitte davor. Gewertet wird der Endzustand (wo eine Nachricht liegt, ihre Flags, neue Entwürfe mit Text und Bezug), dass sonst nichts im Postfach verändert wurde, und die gesprochene Antwort.
- `claude_check.py` stellt dieselben Bitten Claude über `claude -p`, eine nach der anderen gegen die zurückgesetzte Welt. Es läuft immer mit `--strict-mcp-config` und `--setting-sources local`, sonst lädt Claude Mail-Server aus den eigenen Einstellungen.
- `texts/` enthält die Generatoren der Varianten (`make_v1.py` bis `make_v3f.py`), ihre Ergebnisse und `make_ablations.py` für die Ablation.

## Aufruf

Gebraucht werden ein Checkout von Otomo mit Release-Build (`otomo-probe`) und seiner `config.toml`, Ollama mit Gemma 4 E4B, Podman und ein gebautes imap-mcp-rs. Den Checkout nennt `OTOMO_DIR`, ohne die Variable liegt er als `otomo` neben diesem Repository. Gefragt wird als die Identität `OTOMO_IDENTITY` der Konfiguration, ohne die Variable `Christian`.

```
python3 -I welt.py config
python3 -I run.py TAG [--runs N] [--set cases|held|extra] [--config config-voice.toml] [--binary PFAD] [--texts texts/X.json] [--only REGEX]
python3 -I run.py TAG --judge
python3 -I claude_check.py TAG [--set cases|held] [--config config.toml] [--binary PFAD]
python3 -I why.py TAG "REGEX"
python3 -I kick.py [--binary PFAD]
```

Gemma misst man mit `--config config-voice.toml`, Claude mit `config.toml` (`tool_set = "full"`). Ein Durchgang des Katalogs dauert mit Gemma rund acht Minuten. Gemessen wird nie parallel auf Ollama, das stört Gemmas Drafter. Auch zwei Läufe zugleich gegen die Welt gehen nicht, weil jeder Lauf sie zurücksetzt, `claude_check.py` ebenso.
