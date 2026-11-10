# Ergebnis der Messung für imap-mcp-rs

Stand 10.11.2026, Messkit in diesem Ordner (README.md). Gemessen wurde Gemma 4 E4B in Otomo (Ollama, gemma4-e4b-mtp, 32k Kontext) gegen die Welt aus `welt.py`, dazu Claude Sonnet 5.5 als Gegenprobe. Vorher meint die Texte von 2.0.0 vor `tool_set` (heute `full`). Nachher meint für Gemma `tool_set = "voice"` und für Claude `full`, beides mit allen Serveränderungen aus diesem Loop und gemessen gegen das gebaute Binary ohne Nachbildung. Den Stand danach, eine Oberfläche ohne `tool_set`, beschreibt der Abschnitt "Eine Oberfläche für Gemma und Claude", was 3.0.1 und 3.1.0 an den Antworten geändert haben der Abschnitt "Die Antworten (3.0.1 und 3.1.0)".

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

## Die Antworten (3.0.1 und 3.1.0)

In der Nacht zum 10.11.2026 lief der Katalog mit 3.0.1 noch einmal. Die Mitschnitte in `work/` zeigten dabei, wo der Server Gemma etwas erschließen ließ, das er auch hätte sagen können. 3.0.1 behebt außerdem einen Fehler, der älter ist als die Messung: `list_emails` mit `unread_only` lieferte bei einem Konto die älteste ungelesene Mail zuerst, in den Mitschnitten 14 Mal. Seit 3.0.1 übernimmt eine Überarbeitung mit `replaces_uid` auch `to`, `cc` und `subject` des alten Entwurfs, vorher brauchte Gemma dafür einmal drei Anläufe. Ein Kontoname als Ordner ("Arbeit") bekommt einen Hinweis auf das Konto.

3.1.0 wurde als `run/bin/imap-mcp-rs-eine-oberflaeche-4` gemessen und ändert keinen Text, nur Antworten:

- Die neuesten Mails nach einer leeren Suche (`newest`) tragen den Anfang ihres Textes.
- Jede Entwurfsantwort sagt direkt nach `status`, dass nichts verschickt wurde ("saved": "as a draft in Drafts; nothing was sent, the user sends it").
- `list_emails` mit `unread_only` nennt seine Zahl auch `unread`, vor `total`.
- Findet eine Suche nichts und ist ein Suchwort ein Kontoname, beginnt der Hinweis mit dem Weg zu diesem Konto.

| Messung | 3.0.1 | 3.1.0 | Claude auf 3.1.0 |
|---|---|---|---|
| Katalog (33 Bitten, 3 Läufe) | 86/99 | 94/99 | 28/28 |
| zurückgehaltene Bitten (14, 2 Läufe) | 26/28 | 25/28 | 12/12 |
| `NUR_LESEN` (14 Bitten, 2 Läufe) | 24/28 | 26/28 | 11/11 |
| frischer Satz (7 Bitten, 2 Läufe) | 14/14 | 13/14 | 6/6 |
| frischer Satz nur lesend (6 Bitten, 2 Läufe) | 10/12 | 8/12 | 5/5 |

Den größten Sprung machte "Was braucht die Steuerberaterin noch von mir?" mit 0/3 auf 3/3. Gemma findet Selin Kaya in `newest` und liest die Antwort jetzt gleich aus dem Textanfang ("... brauche ich bis zum 30.11. noch: - die Lohnsteuerbescheinigung 2025 ..."). Ebenso stiegen "Hat Jonas auf meine Antwort reagiert?" und "Hab ich was von Amazon bekommen? / Lösch die Mail." von 1/3 auf 3/3, "Schick die Antwort jetzt ab." von 2/3 auf 3/3 und die beiden Absagen beim Antworten nur lesend von 1/2 auf 2/2. "Leite die Rechnung von den Stadtwerken an die Steuerberaterin weiter." kam von 0/3 auf 1/3, zweimal schrieb Gemma den Entwurf und sagte trotzdem "weitergeleitet".

Bei zwei oder drei Läufen fielen "Markier die Rechnung von den Stadtwerken als wichtig" (3/3 auf 1/3), "Was wollte Ben von mir?" (3/3 auf 2/3), "Leg die Telekom-Rechnung zurück in den Posteingang." (2/2 auf 1/2), "Leite das an Jonas weiter" (2/2 auf 1/2), "Verschieb die Mail vom Radhaus in den Papierkorb" (2/2 auf 1/2, eine leere Antwort aus Otomo) und nur lesend "Leite die Urlaubsgenehmigung an Mama weiter." (1/2 auf 0/2). Keiner dieser Fehlgriffe hängt erkennbar an den vier Änderungen. Mit je sechs Läufen wiederholt kamen Weiterleiten und Papierkorb nur lesend mit 3.1.0 auf 11/12 gegen 10/12 mit 3.0.1. Die Stadtwerke-Markierung kam auf 6/6 gegen 4/6, wobei 3.0.1 zweimal beide Rechnungen markierte.

Zur Bestätigung lief 3.1.0 aus dem nix profile je einmal. Gemma kam im Katalog auf 32/33, einmal führte sie "Markier die Mail von Amazon wieder als ungelesen" nicht aus. Bei `NUR_LESEN` kam sie auf 13/14, die eine Lücke ist wieder "Muss ich am Samstag mit irgendwelchen Ausfällen rechnen?" mit der Gegenfrage nach dem Ort. Claude kam auf 28/28 und 11/11. Otomo läuft seit dem 10.11.2026 um 03:13 Uhr mit 3.1.0.

## Was gewirkt hat

1. **Die Zeile des Torwächters.** Otomo fragt vor jeder Antwort Gemma selbst, ob die Bitte ein Werkzeug braucht, und zeigt ihr dafür von jedem Werkzeug nur den Namen und die ersten 140 Zeichen der Beschreibung. Die alten Beschreibungen begannen mit Technik ("Search emails by combinable criteria ..."). Bei "Was schreibt Mama?", "Wann kommt mein Paket?" oder "Antworte Mama: ..." sagte der Torwächter deshalb nein und Gemma antwortete, ohne nachzusehen. Die Beschreibungen in `voice` beginnen mit dem, was der Nutzer davon hat, und nennen in den ersten 140 Zeichen Inhalte wie Rechnungen, Pakete und Termine. Ohne die neuen Beschreibungen fiel "Wann kommt mein Paket?" wieder auf 0/2.
2. **Die Größe.** `voice` blendet aus, was per Sprache kaum gebraucht wird, und kürzt die Texte. Der Prompt je Modellaufruf halbiert sich, Gemma bleibt weit unter ihrem Fenster von 32k.
3. **Instructions unter Otomos 1000 Zeichen.** Otomo gibt einem Modell höchstens 1000 Zeichen der Instructions weiter, von den alten 1586 fehlte der Teil über Entwürfe, uids und retryable. Mit den neuen kamen 88/99, mit den alten gekürzten 87/99, bei den Entwürfen aber 12/18 gegen 9/18.
4. **Kein dry_run in `voice`.** Mit der alten Beschreibung ("returns a preview payload the LLM can show the user for confirmation") fragte Gemma vor jedem Löschen "Soll ich diese E-Mail löschen?". Otomo fragt bei `delete_email` wegen `destructiveHint` ohnehin selbst. Mit den übrigen neuen Texten zusammen war der Unterschied am Ende nicht mehr messbar (7/8 gegen 7/8).

Die Serveränderungen für Ordnernamen per Anzeigename oder Rolle, den Ordnerfehler mit Liste, den Empfängerfehler mit nächstem Schritt und die Ortszeit zeigten gezielt gemessen (je vier Läufe auf den Bitten, die sie betreffen) keinen Unterschied mehr, sobald die Texte sagen, wie es geht. Die Suche mit mehreren Wörtern war nur in der Ablation über den ganzen Katalog dabei (zwei Läufe, 58/64 ohne gegen 53/64 mit ihr, im Rauschen) und zeigte dort ebenfalls keinen Vorteil. Vor den neuen Texten scheiterten genau diese Bitten. Sie bleiben, weil sie für jeden Client richtig sind und nichts kosten. Ordnernamen und Ordnerfehler ersetzen sich dabei gegenseitig: "Schieb den Newsletter in den Papierkorb" ging mit einem der beiden 4/4. Für den Ordnerfehler war der letzte Satz entscheidend: Mit "To delete, use delete_email; it moves to the trash." sagte Gemma dem Nutzer, er müsse den Ordner wechseln, mit "To move emails to the trash, call delete_email with the same folder and uids; it puts them there." tat sie es selbst. Die Ortszeit fing Gemma unter UTC über `date_original` auf, das Mails aus anderen Zonen oder mit UTC im Header nicht haben.

Die Regel für Wörter mit Bindestrich (Teile ab drei Buchstaben, "Telekom-Rechnung" sucht "Telekom" und "Rechnung") kam als Letztes, gegen das Binary von 03:41 mit je vier Läufen: "Leg die Telekom-Rechnung zurück in den Posteingang." 4/4 (ohne die Regel in der Schlussmessung 0/2). Die beiden eigens dafür geschriebenen Bitten ("Was stand in der Amazon-Bestellung?", "Markier die Stadtwerke-Rechnung als wichtig.") gingen mit und ohne die Regel 8/8, Gemma sucht dort nicht mit dem ganzen Wort.

## Verworfen

- **Die Mail im Fokus**, also die Mail des letzten Zugs als Ziel, wenn `uids` fehlt. Bei "Wann kommt mein Paket?" und danach "Markier die Mail als wichtig." fragte Gemma "Welche Mail?", ohne ein Werkzeug zu rufen, mit und ohne Fokus 0/4.
- **`matched` vor `total` bei mehreren Konten.** Auf "Wie viele ungelesene Mails habe ich gerade?" nannte Gemma einmal "15 ungelesene", die Zahl aller Mails beider Posteingänge (`total`). Mit `matched` vor `total`, per Proxy nachgebildet, kam gezielt 7/8 wie ohne (`texts/matched-first.json`). Die Reihenfolge bleibt. In 3.1.0 steht stattdessen `unread` vor `total`, als Teil der vier Änderungen gemessen.
- **"Rows come newest first: when the user means one email, it is the first row."** am Ende der Beschreibung von `search_emails`, per Proxy über 3.1.0 nachgebildet (`texts/newest-first.json`). Die Stadtwerke-Markierung kam mit und ohne den Satz auf 6/6 bei je sechs Läufen. Der Katalog kam mit ihm auf 63/66 bei zwei Läufen (ohne 94/99 bei drei), die zurückgehaltenen Bitten wie ohne auf 25/28. Der Satz bringt nichts, was 3.1.0 nicht schon hat.
- **Eine Umbenennung von list_drafts.** Gemma ruft das Werkzeug als "list_draft" oder "list_draft_emails" auf. Das gehört in Otomo, das bei einem unbekannten Namen die ähnlichsten vorschlägt und dabei "draft" und "drafts" für verschiedene Wörter hält.

## Grenzen, die kein Servertext löst

- "Ergänz im Entwurf an Frau Kaya ..." 0/3: der verstümmelte Name von `list_drafts`, danach gibt Gemma ohne Antwort auf. Das ist ein Punkt für Otomo.
- Eine Folgefrage ohne Anker im Gesagten ("Markier die Mail als wichtig" nach einer Antwort, die die Mail nicht nennt): Otomos Verlauf behält nur Gesprochenes, Gemma fragt nach.
