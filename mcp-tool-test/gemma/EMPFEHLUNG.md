# Ergebnis der Messung für imap-mcp-rs

Stand 09.11.2026, Messkit in diesem Ordner (README.md). Gemessen wurde Gemma 4 E4B in Otomo (Ollama, gemma4-e4b-mtp, 32k Kontext) gegen die Welt aus `welt.py`, dazu Claude Sonnet 5.5 als Gegenprobe. Vorher meint die Texte von 2.0.0 vor `tool_set` (heute `full`). Nachher meint für Gemma `tool_set = "voice"` und für Claude `full`, beides mit allen Serveränderungen aus diesem Loop und gemessen gegen das gebaute Binary ohne Nachbildung. Den Stand danach, eine Oberfläche ohne `tool_set`, beschreibt der Abschnitt "Eine Oberfläche für Gemma und Claude".

## Ergebnis

| Messung | vorher | nachher |
|---|---|---|
| Gemma, Katalog | 34/62 (31 Bitten, 2 Läufe) | 84/99 (33 Bitten, 3 Läufe) |
| Gemma, zurückgehaltene Bitten (14, 2 Läufe) | 11/28 | 25/28 |
| Prompt je Modellaufruf, Median | 15,6k Token | 8,8k Token |
| tools/list | 50 KB | 15,9 KB |
| Claude, Katalog ohne Folgefragen | 27/28 | 28/28 |
| Claude, zurückgehaltene ohne Folgefragen | 12/12 | 12/12 |

Gemma nach Gruppen, vorher gegen nachher: Lesen 15/24 gegen 35/39, Ordnen 6/14 gegen 18/21, Entwürfe 2/10 gegen 11/18, Folgefragen 5/8 gegen 11/12, Mail mit Anweisungen an die KI 4/4 gegen 6/6, Bitte nicht für den Mail-Server 2/2 gegen 3/3.

## Eine Oberfläche für Gemma und Claude

Am Abend des 09.11.2026 hat Christian entschieden, dass es nur noch eine Oberfläche gibt. `tool_set` fällt weg, die in `voice` ausgeblendeten Parameter sind mit einzeiligen Texten zurück und Claude sieht dasselbe wie Gemma. Werkzeuge, die kein Konto benutzen darf, fehlen in der Liste. Ohne `account` lesen die lesenden Werkzeuge alle Konten und zählen jedes Konto vor den Zeilen. Gemessen wurde gegen die Binaries `run/bin/imap-mcp-rs-eine-oberflaeche-1` bis `-3` ohne Nachbildung. `NUR_LESEN` sind 14 neue Bitten mit `config-nur-lesen.toml`, zwei Konten, die beide nur lesen dürfen, so wie bei Christian in Otomo.

| Messung | Gemma | Claude |
|---|---|---|
| Katalog mit `config.toml` (33 Bitten, 3 Läufe) | 86/99 (`voice`: 84/99) | 28/28 ohne Folgefragen |
| zurückgehaltene Bitten (14, 2 Läufe) | 26/28 (`voice`: 25/28) | 12/12 |
| `NUR_LESEN` (14 Bitten, 2 Läufe) | 24/28 (`-2`: 25/28) | 11/11 |
| frischer Satz mit `config.toml` (7 Bitten, 2 Läufe) | 14/14 | |
| frischer Satz nur lesend (6 Bitten, 2 Läufe) | 9/12 (`-2`: 10/12) | |
| Prompt je Modellaufruf mit Werkzeugen, Median | 10.054 Token, nur lesend 6.836 | |
| tools/list | 19,9 KB, nur lesend 8,9 KB | |

Die zurückgekehrten Parameter kosten gegenüber `voice` rund 1.200 Token je Modellaufruf und keinen Treffer. In 210 Aufrufen aus Katalog und zurückgehaltenen Bitten setzte Gemma zweimal `compact`, `dry_run` nie.

Bei Konten, die nur lesen dürfen, entschied ein Satz der Instructions. Gezielt gemessen auf den Absage- und Folgebitten, je vier Läufe:

- "Reading only: nothing can be drafted, moved, marked or deleted; say so when the user asks for it." kam auf 12/16. Gemma sagte etwa "Ich antworte ihr, dass wir am Sonntag gerne kommen." ohne jeden Aufruf.
- "Reading only: to reply, write, move, mark or delete, tell the user that you can only read their mail here." kam auf 16/16. Der Satz nennt die Handlungen so, wie der Nutzer sie sagt.
- Der frische Satz zeigte danach, dass Weiterleiten in der Aufzählung fehlte. Mit "to reply, write, forward, move, mark or delete" kamen sieben Absage- und Folgebitten auf 27/28 statt 25/28. Diese Ergänzung ist am frischen Satz entstanden, er ist für sie also nicht mehr unabhängig. Sie steht in `-3`.

Mit zwei Läufen schwanken die Zahlen um einen bis zwei Treffer, gegen `-3` kamen `NUR_LESEN` und der frische Satz je einen Treffer unter `-2` heraus. Die gezielten Läufe mit vier Wiederholungen sprechen für `-3`. Etwa eines von vier Malen geht eine Folgefrage daneben. Nach "Was schreibt Mama?" behauptet Gemma auf "Schreib ihr zurück, dass wir kommen." dann, sie habe geantwortet. Otomo prüft solche Antworten mit seinem Torwächter, der wertete "Ich habe eine E-Mail an die Personalabteilung formuliert ... Möchtest du, dass ich diese E-Mail jetzt versende?" aber nicht als Behauptung. Das ist ein Punkt für Otomo. In Otomo gab Gemma "Antworte Mama ..." zudem einmal an einen Agenten weiter, statt gleich abzusagen. "Muss ich am Samstag mit irgendwelchen Ausfällen rechnen?" nennt keine Mail, Gemma fragt dort zu Recht nach dem Ort.

## Was gewirkt hat

1. **Die Zeile des Torwächters.** Otomo fragt vor jeder Antwort Gemma selbst, ob die Bitte ein Werkzeug braucht, und zeigt ihr dafür von jedem Werkzeug nur den Namen und die ersten 140 Zeichen der Beschreibung. Die alten Beschreibungen begannen mit Technik ("Search emails by combinable criteria ..."). Bei "Was schreibt Mama?", "Wann kommt mein Paket?" oder "Antworte Mama: ..." sagte der Torwächter deshalb nein und Gemma antwortete, ohne nachzusehen. Die Beschreibungen in `voice` beginnen mit dem, was der Nutzer davon hat, und nennen in den ersten 140 Zeichen Inhalte wie Rechnungen, Pakete und Termine. Ohne die neuen Beschreibungen fiel "Wann kommt mein Paket?" wieder auf 0/2.
2. **Die Größe.** `voice` blendet aus, was per Sprache kaum gebraucht wird, und kürzt die Texte. Der Prompt je Modellaufruf halbiert sich, Gemma bleibt weit unter ihrem Fenster von 32k.
3. **Instructions unter Otomos 1000 Zeichen.** Otomo gibt einem Modell höchstens 1000 Zeichen der Instructions weiter, von den alten 1586 fehlte der Teil über Entwürfe, uids und retryable. Mit den neuen kamen 88/99, mit den alten gekürzten 87/99, bei den Entwürfen aber 12/18 gegen 9/18.
4. **Kein dry_run in `voice`.** Mit der alten Beschreibung ("returns a preview payload the LLM can show the user for confirmation") fragte Gemma vor jedem Löschen "Soll ich diese E-Mail löschen?". Otomo fragt bei `delete_email` wegen `destructiveHint` ohnehin selbst. Mit den übrigen neuen Texten zusammen war der Unterschied am Ende nicht mehr messbar (7/8 gegen 7/8).

Die Serveränderungen für Ordnernamen per Anzeigename oder Rolle, den Ordnerfehler mit Liste, den Empfängerfehler mit nächstem Schritt und die Ortszeit zeigten gezielt gemessen (je vier Läufe auf den Bitten, die sie betreffen) keinen Unterschied mehr, sobald die Texte sagen, wie es geht. Die Suche mit mehreren Wörtern war nur in der Ablation über den ganzen Katalog dabei (zwei Läufe, 58/64 ohne gegen 53/64 mit ihr, im Rauschen) und zeigte dort ebenfalls keinen Vorteil. Vor den neuen Texten scheiterten genau diese Bitten. Sie bleiben, weil sie für jeden Client richtig sind und nichts kosten. Ordnernamen und Ordnerfehler ersetzen sich dabei gegenseitig: "Schieb den Newsletter in den Papierkorb" ging mit einem der beiden 4/4. Für den Ordnerfehler war der letzte Satz entscheidend: Mit "To delete, use delete_email; it moves to the trash." sagte Gemma dem Nutzer, er müsse den Ordner wechseln, mit "To move emails to the trash, call delete_email with the same folder and uids; it puts them there." tat sie es selbst. Die Ortszeit fing Gemma unter UTC über `date_original` auf, das Mails aus anderen Zonen oder mit UTC im Header nicht haben.

Die Regel für Wörter mit Bindestrich (Teile ab drei Buchstaben, "Telekom-Rechnung" sucht "Telekom" und "Rechnung") kam als Letztes, gegen das Binary von 03:41 mit je vier Läufen: "Leg die Telekom-Rechnung zurück in den Posteingang." 4/4 (ohne die Regel in der Schlussmessung 0/2). Die beiden eigens dafür geschriebenen Bitten ("Was stand in der Amazon-Bestellung?", "Markier die Stadtwerke-Rechnung als wichtig.") gingen mit und ohne die Regel 8/8, Gemma sucht dort nicht mit dem ganzen Wort.

## Verworfen

- **Die Mail im Fokus**, also die Mail des letzten Zugs als Ziel, wenn `uids` fehlt. Bei "Wann kommt mein Paket?" und danach "Markier die Mail als wichtig." fragte Gemma "Welche Mail?", ohne ein Werkzeug zu rufen, mit und ohne Fokus 0/4.
- **`matched` vor `total` bei mehreren Konten.** Auf "Wie viele ungelesene Mails habe ich gerade?" nannte Gemma einmal "15 ungelesene", die Zahl aller Mails beider Posteingänge (`total`). Mit `matched` vor `total`, per Proxy nachgebildet, kam gezielt 7/8 wie ohne (`texts/matched-first.json`). Die Reihenfolge bleibt.
- **Eine Umbenennung von list_drafts.** Gemma ruft das Werkzeug als "list_draft" oder "list_draft_emails" auf. Das gehört in Otomo, das bei einem unbekannten Namen die ähnlichsten vorschlägt und dabei "draft" und "drafts" für verschiedene Wörter hält.

## Grenzen, die kein Servertext löst

- "Ergänz im Entwurf an Frau Kaya ..." 0/3: der verstümmelte Name von `list_drafts`, danach gibt Gemma ohne Antwort auf. Das ist ein Punkt für Otomo.
- Eine Person nach ihrer Rolle ("Was braucht die Steuerberaterin noch von mir?"): Über `newest` findet Gemma Selin Kaya, liest die Mail dann aber nicht und fragt nach (0/3).
- Zwei Treffer, von denen der neueste gemeint ist ("Markier die Rechnung von den Stadtwerken als wichtig", 1/3): Die Suche über alle Ordner findet auch die Rechnung vom September in Rechnungen und Gemma nimmt sie.
- Eine Folgefrage ohne Anker im Gesagten ("Markier die Mail als wichtig" nach einer Antwort, die die Mail nicht nennt): Otomos Verlauf behält nur Gesprochenes, Gemma fragt nach.
