MX(1) "github.com/paulchambaz/matrix-tui" "General Commands Manual"

# NAME

*mx* - keyboard-driven terminal client for Matrix

# SYNOPSIS

*mx* [*-hV*]++
*mx* *server list* [*--json*]++
*mx* *channel list* [*-s* _server_] [*-t* _team_] [*-k* _kind_] [*-c* _channel_] [*--json*] [_query_]++
*mx* *message list* [*-s* _server_] [*-t* _team_] [*-k* _kind_] [*-c* _channel_] [*-n* _number_] [*--json*] [_query_]

# DESCRIPTION

*mx* is a terminal client for Matrix. It keeps several homeservers and spaces
open at once, shows new messages as they arrive, tells you what is unread and
where you were mentioned, and lets you write messages in your own editor.

The screen has four parts. The channels of the current team are tabs along the
top: the current one is inverted, unread ones are bold with a dot, and a
channel where you were mentioned ends with *!*. A room muted in another client
stays quiet, and one set to mentions only is marked just for mentions. Below is the conversation, a
plain log with the time, the author and the text of each message. At the
bottom, the input row appears when you type, and a status line shows the mode,
where you are, who is typing, your position in the channel and how to get help.

Keys are single letters, with no sequences. They act on the selected message,
the one highlighted in the log. Channels are written *#name* when public,
*+name* when private and *@name* for direct and group conversations.

# OPTIONS

*-h*, *--help*
	Print a short help and exit. After a command, print the help of that
	command.

*-V*, *--version*
	Print the version and exit.

# GETTING STARTED

Start *mx* and type *:new-server*. It asks, one question at a time, for a
name, the address of your homeserver, your login and your password. Once the
login works, the server is saved and opened. Repeat it to add more servers.

The spaces you are in are the teams. *Home* holds the rooms that belong to no
space, and your direct messages show in every team. Rooms are the channels.
The first time, *mx* creates a device for itself and keeps its session in
_~/.local/share/mx_, so the next starts do not log in again.

The label at the left of the tabs shows the server and the team you are in.
*H* and *L* move
between the teams of all your servers, *J* and *K* between the channels of the
current team, and *f* finds a channel by name.

Press *?* at any time for the list of keys.

# COMMAND LINE

Without a command, *mx* starts the terminal interface. With one, it prints to
the standard output and exits. It never draws on the terminal and never asks
anything, so a script or an agent can run it. The secrets come from
_servers.toml_, including *password_cmd*.

The commands are read-only. They do not post, edit, delete, react, change the
status, send typing events, or mark a channel as read, so running one twice
gives the same unread messages; use *after* to ask only for what is new. There
is deliberately no command that marks a channel as read.

Without options, a command works on all servers and all teams. A command or a
noun may be shortened to any unambiguous prefix (*mx m l*), and a noun may be
written in the plural (*mx messages list*). Scripts should spell them out.

*server list*
	The configured servers. It does not touch the network.

*channel list*
	The channels with their unread and mention counts. Use it to find what
	to ask for.

*message list*
	The messages of the matching channels, oldest first. This is the dump
	command: reading what is unread is only a query on it.

## Scope

The scope says where to look. It decides what is fetched, so it is given as
options. An option may be given several times, or as a comma list, to mean
"or".

*-s* _server_
	Only this server, by its name in _servers.toml_.

*-t* _team_
	Only this team, by its name. Direct and group messages belong to no
	team, so this option does not apply to them; use *-k* to leave them out.

*-k* _kind_
	Only these kinds of channel: _public_, _private_, _direct_ or _group_.
	_dm_ stands for _direct_ and _group_.

*-c* _channel_
	Only this channel, by its name or its display name, ignoring case and
	never fuzzy. _@alice_ is the direct message with alice. A name that
	matches several channels of one team is an error that lists them.

*-n* _number_
	For *message list*, the last _number_ messages of each channel, 0 for
	all. The default is 50, and a notice on the standard error says which
	channels were cut. A query that holds *unread* is never cut by the
	default, because dropping unread messages silently loses information.

*--json*
	Print JSON instead of text. See *OUTPUT*.

## Query

What follows the options is a query: clauses joined by *and*, each possibly
preceded by *not*, and groups of them joined by *or*, which binds looser than
*and*. A message must satisfy every clause of one group, so
_dm and unread or mention and unread_ means (dm and unread) or (mention and
unread). There is no parenthesis. The words are those of
himalaya(1) where they exist.

*from* _user_[,_user_]
	Written by one of these users. _me_ is the account that logged in, on
	each server.

*mention*
	Calls you: it names you, or _@all_, _@channel_ or _@here_, or it comes
	from someone else in a direct or group channel.

*dm*
	In a direct or group channel, as *-k dm*.

*unread*
	Not read yet: newer than the last time you viewed the channel, and not
	yours.

*after* _time_, *before* _time_
	Newer or older than a time. A time is an RFC 3339 date or time
	(_2026-11-04_, _2026-11-04T09:00_) or a duration back from now (_90m_,
	_12h_, _7d_).

*body* _text_
	Contains the text, ignoring case. Quote it if it holds spaces.

*thread* _id_
	The message with this event id and the replies in its thread; plain replies are not listed.

*channel list* takes only *unread* and *mention*, which then select the
channels that have such messages, *or* included. Any other clause is an error.

## Output

Messages are sorted from oldest to newest, and channels like the interface
does: latest activity first. Nothing is colored. System messages (joins,
leaves, renames) are never printed.

With text, each channel opens with a header line, then each message takes one
line:

```
== work/dev #general  (3 unread, 1 mention) ==
 2026-11-04 09:12  alice  did the deploy go through?
*2026-11-04 09:14  bob    yes, see the thread
!2026-11-04 09:15  bob    (re alice) ping @paul
```

The first column holds *\** for an unread message and *!* for one that calls
you. Times are local, 24-hour. Attachments appear as _[file: name]_ and
reactions are not shown. *server list* and *channel list* print an aligned
table with a header row.

With *--json*, the output is one array. A message holds _server_, _team_,
_channel_, _channel_id_, _kind_, _id_, _root_id_ (empty when it is not a
reply), _time_ (RFC 3339, UTC), _user_, _text_ (the markdown as typed),
_unread_, _mentions_me_, _edited_, _files_ and _reactions_. A channel holds
_server_, _team_, _channel_, _channel_id_, _kind_, _unread_ and _mentions_. A
server holds _name_ and _url_. Every object stands alone; *jq -c '.[]'* gives
one per line.

## Errors

Errors go to the standard error as _mx: code: message_, or as a JSON object
with _code_ and _message_ under *--json*. The codes are stable: _usage_,
_bad-query_, _unknown-server_, _unknown-team_, _unknown-channel_,
_ambiguous-channel_, _no-credentials_, _login-failed_,
_unreachable_ and _server-error_.

A server that fails is reported and skipped, so the others still answer.

# KEYS

*l h*
	Next and previous message. At the top of the channel, *h* loads older
	messages.

*j k*
	Scroll the log down and up by a line, without moving the selected
	message. While it is off the screen, *h* and *l* select the nearest
	message on the screen instead of moving.

*g G*
	First and last message. *g* also loads older messages.

*w b*
	Next and previous group of messages from the same author.

*J K*
	Next and previous channel.

*H L*
	Previous and next team, across servers.

*:*
	Open the command line. See *COMMANDS*.

*?*
	Show the help. *j* and *k* scroll it, *esc* closes it.

*n*
	Go to the next unread message, then to the next channel with unread
	messages.

*N*
	Go to the next message that mentions you, in any team and any server
	already loaded. Teams never shown are loaded in the background, one per
	second.

*c*
	Mark the open channel as read.

*f F*
	Find a channel. *f* looks in the current team, *F* in every team of
	every server. See *FINDER*.

*i*
	Write a message. *enter* sends it, *esc* leaves and keeps the text for
	next time. See *WRITING*.

*ctrl-g*
	Write the message in your editor, in the lower half of the screen.

*r*
	Reply to the selected message, inside its thread when it is in one.

*e*
	Edit the selected message, if it is yours.

*d*
	Delete the selected message, if it is yours. *y* confirms.

*a*
	Show or hide the reactions under the messages.

*x X*
	React to the selected message. *x* shows the usual reactions on the
	status line and *1* to *9* picks one. *X* repeats the last one. Reacting
	again with the same emoji removes your reaction. Showing the reactions
	is turned on for you.

*V*
	Select a range of messages, then *h* and *l* extend it.

*y Y*
	Copy the selected message or range. *Y* adds the author and the time of
	each message.

*p*
	Paste what was copied into the input.

*m*
	Show the users of the channel with their presence. *j* and *k* scroll
	the column, *m* hides it.

*o*
	Open the links and attachments of the selected message. See *LINKS AND
	ATTACHMENTS*.

*q*
	Quit.

# FINDER

*f* and *F* put a search line on the input row and the matching channels on
the status line, like a wildmenu.

_words_
	Channels whose name contains every word. With *F*, the name of the
	server and the team count too.

_@x_
	Direct and group conversations matching _x_.

_a#b_
	The channel _b_ of the team _a_. *#b* is the channel _b_ in any team,
	*a#* is the team _a_ alone.

*tab* and the arrow keys choose, *enter* goes there, *esc* closes.

# COMMANDS

A command is typed after *:* and run with *enter*. As in vi, a command can be
shortened to the fewest letters that tell it from the others: *:ser* is *server*,
*:rel* is *reload*, *:q* is *quit*. When the letters fit several commands, they
are listed instead. The status line shows the commands that begin with what you
typed, and *tab* writes the first one in full, then the next ones, *shift-tab*
going back. Names of servers, teams and
users are matched without regard to case: a whole name wins, otherwise a part of
a name works when only one name has it.

*server*, *team*, *remove-server* and *dm* show the names that fit what you type on
the status line, like the finder does. *tab* and the arrow keys choose and
*enter* takes the chosen name, the first one if you did not choose. For *dm*
the names are the users *mx* already knows, such as the authors of messages
you have seen and the people you have talked to.

*new-server*
	Log in to another server.

*server* _name_
	Go to a server.

*team* _name_
	Go to a team of the current server.

*go* _words_
	Open the best channel matching the words, in the current team, as *f* does.
	See *FINDER*.

*remove-server* _name_
	Forget a server: it is taken out of _servers.toml_ and closed. Nothing is
	changed on the server itself.

*dm* _user_...
	Open the direct conversation with a user, creating it if needed. A user is
	written _@alice:example.org_, or just _alice_ on your own homeserver.
	With several users, create a group conversation with them.

*create* [*public*] _name_
	Create a room and open it. It is invite-only and encrypted, unless
	*public* comes first: then anyone can join and it is not encrypted.
	Encryption cannot be turned off afterwards.

*space* *new* _name_, *space* *add* _space_, *space* *remove*
	Spaces are the teams. *new* creates a space that only invited users
	join. *add* puts the open room in a space, found by the name you type,
	and *remove* takes it out of the space of the current team. Only
	people with the right to edit the space can do it. Run *reload* if the
	room does not move at once.

*invite* _user_
	Invite a user into the open room.

*kick* _user_
	Remove a user from the open room. They can come back if invited.

*ban* _user_
	Remove a user from the open room and bar them from joining it.

*unban* _user_
	Let a banned user join the open room again.

*rename* _name_
	Give the open room a new name.

*nick* _name_
	Change the name you go by on your homeserver, in every room.

*ignore* _user_
	Hide everything a user says, in every room, from now on. Tab completes
	names. The server does the hiding, so it holds on your other devices.

*unignore* _user_
	Show a user again.

*report* [_reason_]
	Report the selected message to the moderators of the server.

*search* _text_
	List the messages of the open room that hold the text, ignoring case,
	newest first, in a list where *j* and *k* move, *enter* opens the hit
	in its place in the room and *esc* closes. A hit older than the last
	500 messages cannot be opened, scroll up for it. It reads the last 2000 messages of the room, so it also works in
	encrypted rooms, and it stops early after 50 hits.

*join* [_room_]
	Join a room given by its alias (_#town-square:example.org_), its id
	(_!abc:example.org_), a _https://matrix.to_ or _matrix:_ link to it, or the
	name of a room you can already see. Without
	argument, accept the invitation shown in the open channel. Invitations,
	to rooms and to direct messages alike, appear first in the Home team,
	marked like a mention; nothing is joined until you say so.

*leave*
	Leave the open channel, or decline the invitation it shows. Direct and
	group conversations cannot be left.

*topic* [_text_]
	Show the header and the purpose of the open channel, or with a text set
	the topic of the room.

*reload*
	Fetch the open channel, its messages and the channel list again, for
	example after the connection dropped.

*mark-read*
	Mark the open channel as read, like *c*.

*mark-all-read*
	Mark every unread channel of the current team as read.

*users*, *reactions*, *open*
	The same as *m*, *a* and *o*.

*copy* [*full*]
	The same as *y*, or as *Y* with *full*.

*upload* _path_
	Send a file or an image to the open channel. A leading *~/* means your home
	folder, and Tab completes the path. The message appears in the channel once
	the server has it, and the file is refused if it is larger than the server
	allows.

*location* _latitude_ _longitude_
	Send a place to the open channel. Places and polls sent by others show
	as text.

*react* _name_
	React to the selected message with any emoji known by name, such as
	_tada_. Like *x*, reacting again with the same emoji removes it.

*pin*, *unpin*
	Pin the selected message to its channel, or unpin it.

*status* _online|away|dnd|offline_
	Set your presence, if the homeserver supports it.

*recover* _key_
	Unlock the keys of older encrypted messages with the recovery key of your
	account, so a new device can read them. See *ENCRYPTION*.

*recovery*
	Create the recovery key of the account, or replace it when there is one.
	The key is shown on a page, once: write it down. Any older key stops
	working. See *ENCRYPTION*.

*sessions*
	List the devices logged in to the account, the last seen first.

*logout* _id_
	Log another device out, given its id from *:sessions*.

*password*
	Ask twice for a new password, hidden, and change it. The other sessions are
	logged out. Update the password in _servers.toml_ afterwards, because *mx*
	uses it to sign in again.

*deactivate* _id_
	Close the account for good. The argument must be your full user id, such as
	_@paul:example.org_. The server and its data are forgotten by *mx*.

*verify* [_user_]
	Without a user, ask the other session that was active last to verify this
	one. With a user, ask them to verify their keys with you, in a direct
	conversation that is opened if needed: they must have joined it. Accept the
	request there and compare the seven emojis shown on both sides: *y* if
	they match, *n* or *Esc* if they differ. See *ENCRYPTION*.

*trust* _user_
	Accept the new keys of a user whose identity changed. See *ENCRYPTION*.

*set* _key_ _value_
	Change a setting until you quit: *editor*, *copy*, *notify*, *open.image*,
	*open.video*, *open.browser*, *open.text* or *open.other*. Without a value,
	*editor* and *copy* go back to their automatic choice. Nothing is written to
	_config.toml_.

*help*
	Show the help.

*quit*
	Quit.

# WRITING

*i* opens a line at the bottom of the screen. *enter* sends the message, *esc*
leaves without sending: the text is kept, per channel, until you come back to
it. Pasting keeps line breaks.

*ctrl-g* works in the same way, from the input row or from the log. It starts
your editor on the text, in the lower half of the screen, while the
conversation stays alive above it. When you quit the editor, the text comes
back to the input row, where *enter* sends it. Leaving the editor with an empty
file cancels. The editor is the *editor* setting, else *$EDITOR*, else *vi*.

*r* replies to the selected message, *e* edits it and *d* deletes it. Editing
and deleting only work on your own messages.

Other people see when you are typing, as with the web app. Sending a message
marks the channel as read.

# ENCRYPTION

Encrypted rooms work like the others: *mx* decrypts what it can and sends
encrypted messages. A message that cannot be decrypted is shown as
_[unable to decrypt]_. This happens for messages sent before this device
existed, unless you give it the recovery key of your account with *:recover*.
The recovery key is the one your other clients showed you when you set up
secure backup. To make other clients trust this session, type *:verify* and
accept the request there (it goes to the session used last); a request that another session sends opens the same
page by itself. The account needs cross-signing keys, which another client sets
up.

*:recovery* creates the recovery key, or replaces it, and shows it once. The
commands *:password*, *:logout* and *:deactivate* ask the server to confirm
your password: they only work for a server with a login and a password in
_servers.toml_, otherwise use the account page of your server.

*:verify* _user_ compares emojis with another person. When a person you had
verified shows new keys, *mx* says so when you open a conversation with them,
until you run *:verify* _user_ again or *:trust* _user_. A request from a user arrives as a message in the direct
conversation and opens the same page.

# MARKDOWN AND EMOJI

Messages are shown with a minimal version of Markdown: the markers are hidden
and the text is styled. *\*\*bold\*\**, *\*italic\** and *\_italic\_* (only at
the edge of a word, so snake_case_names are left alone), *~~strikethrough~~*,
*`code`* and fenced blocks, *# headings*, *> quotes* and *[text](url)* links.
A backslash keeps a marker as typed. Lists, tables and images are shown as
they were written.

Mentions are colored: yellow when they are you, *@all*, *@channel* or
*@here*, blue otherwise. System messages such as joins and leaves are muted.

*:smile:* style names are shown as the emoji. Names the terminal font cannot
know, such as custom emoji of the server, stay as they are written.

# LINKS AND ATTACHMENTS

*o* opens what the selected message holds: its links first, then its
attachments. With nothing to open it says so, with one thing it opens it, and
with several it shows them on the status line and *1* to *9* picks one.

A link goes to the *browser* program. An attachment is downloaded first, to a
private folder that is removed when *mx* quits, then opened by the program
for its type: *image* for images, *video* for video and audio, *text* for
text and source files and *other* for the rest. See *CONFIGURATION*.

The *text* program runs inside *mx*, in the lower half of the screen, and you
come back when you quit it. The other programs are started on their own and
keep running if you close *mx*. Large files are downloaded completely before
they are opened.

# CONFIGURATION

Servers are in _servers.toml_, one *[[server]]* table each:

*name*
	How the server is called in the status line.

*url*
	The address of the server.

*method*
	*password* (the default), *sso* or *oidc*. With *sso* or *oidc*, *mx* opens
	the browser (the *open.browser* setting) on the sign-in page of the server
	and takes no login or password. *oidc* is for servers that sign in through
	OAuth 2.0 (the newer servers, such as those with Matrix Authentication
	Service); the session renews itself, but once it expires, restart *mx* to
	sign in again.

*login*
	Your login, for the password forms below.

*password*, *password_cmd*
	Exactly one of them. The password written in the file, or a command whose
	output is the password. The command is run by the shell when *mx* starts.

_config.toml_ holds the settings. Every key is optional and a mistake in a key
name is reported when *mx* starts.

*editor*
	The program *ctrl-g* starts. It may have arguments.

*copy*
	The command that receives the text copied with *y*. Without it, *mx*
	uses *wl-copy*, *xclip* or *xsel*, whichever is installed for your
	session. The terminal is also asked to copy, which is enough in many
	terminals, including over SSH.

*notify*
	A command that rings when a message calls you, or arrives in a direct
	message or a group that is not muted, while another channel is open. The
	title is in *$1* and the text in *$2*, for example
	*notify-send "$1" "$2"*. Without it nothing rings. A message that calls
	you rings even in a muted room.

*[open]*
	A table with the programs for *browser*, *image*, *video*, *text* and
	*other*, as described in *LINKS AND ATTACHMENTS*. Each defaults to
	*xdg-open*. The file or address is added as the last argument.

# ENVIRONMENT

*EDITOR*
	The editor, when *editor* is not set.

*NO_COLOR*
	When set to anything, *mx* uses no colors; emphasis is kept.

*TMPDIR*
	Where attachments are downloaded and messages are written for the
	editor.

# FILES

_~/.config/mx/servers.toml_
	The servers and how to log in to each. Added with *:new-server*, and
	readable only by you.

_~/.config/mx/config.toml_
	The settings above.

_~/.local/share/mx/<name>/_
	The session of a server, with the keys of the account and the messages
	kept for encryption. Readable only by you. Removing the folder makes
	*mx* log in again with a new device.

# EXAMPLES

A _servers.toml_ with a password kept in a password manager, and one in the file:

```
[[server]]
name = "work"
url = "https://matrix.example.org"
login = "paul"
password_cmd = "pass show work/matrix"

[[server]]
name = "club"
url = "https://club.example.net"
login = "paul"
password = "correct horse battery staple"
```

A server that signs in through the browser:

```
[[server]]
name = "matrix"
url = "https://matrix.org"
method = "sso"
```

Two accounts on the same server are two tables with different names:

```
[[server]]
name = "paul"
url = "https://matrix.example.org"
login = "paul"
password_cmd = "pass show matrix/paul"

[[server]]
name = "paul-work"
url = "https://matrix.example.org"
login = "paul.work"
password_cmd = "pass show matrix/work"
```

A _config.toml_ for a Wayland desktop:

```
editor = "nvim"

[open]
image = "imv"
video = "mpv"
browser = "firefox"
text = "nvim -R"
```

Jump to the room _ops_ of the space _infra_: press *f*, type _infra#ops_ and
press *enter*.

What needs your attention, on every server:

	mx message list unread and mention

The direct messages that have unread messages, then the messages themselves:

	mx channel list -k dm unread++
mx message list -k dm unread

One conversation, then only what came after it:

	mx message list -s work -c @alice -n 200++
mx message list -s work -c @alice after 1h

Everything you ever wrote, to study your style:

	mx message list -n 0 --json from me | jq -r '.[].text'

What was said about a topic, and one thread:

	mx message list -s work body deploy and after 7d++
mx message list -s work thread 9x4kq1m7bpg8jrqzcn3o4w5e6y

# EXIT STATUS

*0* on success, even when a command found nothing. *1* when an error stops
*mx*, or when a server could not be reached or refused the login during a
command, whose other servers are still printed. *2* when the command line is
wrong: an unknown option, server, team or channel, or a bad query. The error
is printed on the standard error with its cause.

# AUTHORS

This program was written by Paul Chambaz. It is licensed under the GPLv3, for
more information, please read the *LICENSE* file.
