#!/usr/bin/env python3
"""Folder icons, Material-style: a solid folder with a badge cut out of its bottom-right corner.

Writes assets/icons/folder-<kind>[-open].svg (folder, badge area cut), assets/icons/badge-<kind>.svg
(the badge alone, tinted separately) and src/folder_icons.rs (name -> icons + colors).
Run from crates/app: python3 assets/gen_folder_icons.py
"""
import os

CLOSED = 'M1 3.5A1.5 1.5 0 0 1 2.5 2h3.3l1.6 1.6h6.1A1.5 1.5 0 0 1 15 5.1v7.4A1.5 1.5 0 0 1 13.5 14h-11A1.5 1.5 0 0 1 1 12.5z'
OPEN_BACK = 'M1 12.5V3.5A1.5 1.5 0 0 1 2.5 2h3.3l1.6 1.6h4.6A1.5 1.5 0 0 1 13.5 5.1V6.4H4a1 1 0 0 0-1 .7z'
OPEN_FRONT = 'M1 12.6L3 7.1A1 1 0 0 1 4 6.4h10.4a.7.7 0 0 1 .65.95L13.2 13a1.5 1.5 0 0 1-1.4 1H2.4A1.4 1.4 0 0 1 1 12.6z'

GEAR = [('s', 'M3 1.6a1.4 1.4 0 1 1 0 2.8 1.4 1.4 0 0 1 0-2.8zM3 .1v.6M3 5.3v.6M.1 3h.6M5.3 3h.6M.9.9l.5.5M4.6 4.6l.5.5M.9 5.1l.5-.5M4.6 1.4l.5-.5')]
CLOCK = [('s', 'M3 .4a2.6 2.6 0 1 1 0 5.2A2.6 2.6 0 0 1 3 .4zM3 1.7v1.5l1 .7')]
GLOBE = [('s', 'M3 .4a2.6 2.6 0 1 1 0 5.2A2.6 2.6 0 0 1 3 .4zM.5 3h5M3 .4c-1.1 1.5-1.1 3.7 0 5.2M3 .4c1.1 1.5 1.1 3.7 0 5.2')]
ARROWS = [('s', 'M.5 1.6h5M4.1.3l1.4 1.3-1.4 1.3M5.5 4.4H.5M1.9 3.1L.5 4.4l1.4 1.3')]
BOX = [('f', 'M3 0l3 1.5v3L3 6 0 4.5v-3z')]
CYL = [('f', 'M.5 1.2C.5.5 1.6 0 3 0s2.5.5 2.5 1.2v3.6C5.5 5.5 4.4 6 3 6S.5 5.5.5 4.8z')]
MOUNT = [('f', 'M0 6l2.2-3 1.5 1.8 1-1.2L6 6zM4.6.3a1 1 0 1 1 0 2 1 1 0 0 1 0-2z')]
HASH = [('s', 'M2.2.4l-.6 5.2M4.6.4L4 5.6M.4 2.1h5.3M.2 4h5.3')]
LINES = [('s', 'M.5.8h5M.5 3h5M.5 5.2h3')]
SHIELD = [('f', 'M3 .1l2.7 1v2c0 1.5-1.1 2.6-2.7 2.9C1.4 5.7.3 4.6.3 3.1v-2z')]
LOCK = [('f', 'M.8 2.7h4.4V6H.8z'), ('s', 'M1.8 2.7V1.9a1.2 1.2 0 0 1 2.4 0v.8')]
PUZZLE = [('f', 'M.3 1.6h1.6a1.1 1.1 0 1 1 2.2 0h1.6v1.6a1.1 1.1 0 1 1 0 2.2V6H.3z')]
PLUG = [('s', 'M2 0v1.5M4 0v1.5M1 1.6h4v1.4a2 2 0 0 1-4 0zM3 5v1')]
BELL = [('f', 'M3 .2a2 2 0 0 1 2 2v1.6l.8 1.1H.2L1 3.8V2.2a2 2 0 0 1 2-2zM2.1 5.3h1.8a.9.9 0 0 1-1.8 0z')]
PLAY = [('f', 'M1 .3l4.8 2.7L1 5.7z')]

GLYPHS = {
    'test': [('s', 'M.6 3.2l1.7 1.7 3.3-3.6')],
    'clock': CLOCK,
    'src': [('s', 'M2 1L.4 3 2 5M4 1l1.6 2L4 5')],
    'components': [('f', 'M0 0h2.6v2.6H0zM3.4 0H6v2.6H3.4zM0 3.4h2.6V6H0zM3.4 3.4H6V6H3.4z')],
    'app': [('f', 'M0 0h1.6v1.6H0zM2.2 0h1.6v1.6H2.2zM4.4 0H6v1.6H4.4zM0 2.2h1.6v1.6H0zM2.2 2.2h1.6v1.6H2.2zM4.4 2.2H6v1.6H4.4zM0 4.4h1.6V6H0zM2.2 4.4h1.6V6H2.2zM4.4 4.4H6V6H4.4z')],
    'image': MOUNT,
    'hash': HASH,
    'lines': LINES,
    'gear': GEAR,
    'terminal': [('s', 'M.5 1l2 2-2 2M3.3 5.2h2.4')],
    'arrows': ARROWS,
    'cylinder': CYL,
    'box': BOX,
    'bolt': [('f', 'M3.8 0L.8 3.5h2.1L2.2 6l3.1-3.7H3.2z')],
    'hook': [('s', 'M4.2.4v3a1.9 1.9 0 0 1-3.8 0v-.6M3.4 1.2L4.2.4 5 1.2')],
    'wrench': [('f', 'M4.3.1a1.9 1.9 0 0 0-2 2.5L.2 4.7l1.1 1.1 2.1-2.1a1.9 1.9 0 0 0 2.5-2l-1 1-1.2-.3-.3-1.2z')],
    'globe': GLOBE,
    'type': [('s', 'M.6.9h4.8M3 .9v4.6')],
    'layers': [('f', 'M3 0l3 1.6L3 3.2 0 1.6z'), ('s', 'M.3 3.3L3 4.7l2.7-1.4M.3 4.9L3 6.3l2.7-1.4')],
    'branch': [('f', 'M1 .2a.9.9 0 1 1 0 1.8.9.9 0 0 1 0-1.8zM1 4.1a.9.9 0 1 1 0 1.8.9.9 0 0 1 0-1.8zM5 1.2a.9.9 0 1 1 0 1.8.9.9 0 0 1 0-1.8z'), ('s', 'M1 1.9v2.3M5 2.9c0 1.4-2.6 1-3.6 2')],
    'window': [('s', 'M1 .4h4a.6.6 0 0 1 .6.6v4a.6.6 0 0 1-.6.6H1a.6.6 0 0 1-.6-.6V1A.6.6 0 0 1 1 .4z'), ('f', 'M.4 1h5.2v1.4H.4z')],
    'layout': [('s', 'M1 .4h4a.6.6 0 0 1 .6.6v4a.6.6 0 0 1-.6.6H1a.6.6 0 0 1-.6-.6V1A.6.6 0 0 1 1 .4zM2.4.4v5.2M2.4 3h3.2')],
    'font': [('s', 'M.4 5.6L3 .4l2.6 5.2M1.3 3.8h3.4')],
    'note': [('s', 'M2.2 4.8V.6l3.3-.5v3.8'), ('f', 'M1.2 3.8a1 1 0 1 1 0 2 1 1 0 0 1 0-2zM4.5 3.3a1 1 0 1 1 0 2 1 1 0 0 1 0-2z')],
    'play': PLAY,
    'mail': [('s', 'M.3 1h5.4v4H.3zM.4 1.2L3 3.3l2.6-2.1')],
    'lock': LOCK,
    'shield': SHIELD,
    'pencil': [('f', 'M4.3.3l1.4 1.4-3.8 3.8H.5V4.1z')],
    'bug': [('f', 'M3 1.5c1.2 0 2 .9 2 2.2S4.2 6 3 6 1 5 1 3.7s.8-2.2 2-2.2z'), ('s', 'M0 3h1M5 3h1M.3 5.2l.9-.5M5.7 5.2l-.9-.5M2 .3l.5.8M4 .3l-.5.8')],
    'error': [('s', 'M3 .4a2.6 2.6 0 1 1 0 5.2A2.6 2.6 0 0 1 3 .4zM3 1.7v1.5M3 4.3v.01')],
    'down': [('s', 'M3 .3v4M1.2 2.6L3 4.4l1.8-1.8M.5 5.7h5')],
    'up': [('s', 'M3 4.4v-4M1.2 2.1L3 .3l1.8 1.8M.5 5.7h5')],
    'puzzle': PUZZLE,
    'plug': PLUG,
    'func': [('s', 'M4.6.5c-1 0-1.5.5-1.5 1.5v2.4c0 1-.5 1.5-1.5 1.5M1.6 2.6h3')],
    'container': [('s', 'M.4 1.4h5.2v4H.4zM2.1 1.4v4M3.9 1.4v4')],
    'cloud': [('f', 'M1.6 5.4a1.4 1.4 0 0 1-.2-2.8A2 2 0 0 1 5 2.4a1.5 1.5 0 0 1-.2 3z')],
    'spark': [('f', 'M3 0l.8 2.2L6 3l-2.2.8L3 6l-.8-2.2L0 3l2.2-.8z')],
    'gauge': [('s', 'M.4 4.6a2.6 2.6 0 1 1 5.2 0M3 4.6l1.3-1.7')],
    'archive': [('s', 'M.3.6h5.4v1.6H.3zM.8 2.2v3.2h4.4V2.2M2.3 3.4h1.4')],
    'braces': [('s', 'M2 .4c-.8 0-1.1.3-1.1 1v.9c0 .5-.3.7-.8.7.5 0 .8.2.8.7v.9c0 .7.3 1 1.1 1M4 .4c.8 0 1.1.3 1.1 1v.9c0 .5.3.7.8.7-.5 0-.8.2-.8.7v.9c0 .7-.3 1-1.1 1')],
    'monitor': [('s', 'M.3.6h5.4v3.6H.3zM2 5.6h2M3 4.2v1.4')],
    'server': [('f', 'M.3.4h5.4v2.2H.3zM.3 3.4h5.4v2.2H.3z')],
    'chat': [('f', 'M.5.5h5v3.8H2.3L1 5.6V4.3H.5z')],
    'bell': BELL,
    'graph': [('s', 'M3 .3l2.4 1.4v2.6L3 5.7.6 4.3V1.7zM3 .3L.6 4.3h4.8z')],
    'chart': [('s', 'M.6 5.5h5M1.5 4.4V2.8M3 4.4V.8M4.5 4.4V2')],
    'leaf': [('f', 'M5.7.3C2 .3.3 2.1.3 5.1c0 .2 0 .4.1.6C3.6 5.7 5.7 4 5.7.3z')],
    'cycle': [('s', 'M5.2 2A2.5 2.5 0 1 0 5.5 3.8M5.4.4v1.6H3.8')],
    'check-list': [('s', 'M.4 1.1l.7.7 1.2-1.3M3.3 1.2h2.3M.4 4.1l.7.7 1.2-1.3M3.3 4.2h2.3')],
    'key': [('s', 'M1.9 1.2a1.6 1.6 0 1 1 0 3.2 1.6 1.6 0 0 1 0-3.2zM3.5 2.8h2.3M5 2.8v1.4')],
    'chip': [('s', 'M1.2 1.2h3.6v3.6H1.2zM2.2 0v1M3.8 0v1M2.2 5v1M3.8 5v1M0 2.2h1M0 3.8h1M5 2.2h1M5 3.8h1')],
    'book': [('s', 'M.5.4h1.6v5.2H.5zM2.1.4h1.6v5.2H2.1zM3.9.9l1.5-.4 1.3 4.9-1.5.4z')],
    'doc': [('s', 'M1 .3h2.6L5 1.7v4H1zM3.5.3v1.5H5M2 3.2h2M2 4.4h2')],
    'flag': [('f', 'M.8.3h4.6l-1 1.6 1 1.6H1.6V6H.8z')],
}

# kind: (glyph, color, folder names)
KINDS = [
    ('test', 'test', 0x26a69a, 'test tests __tests__ spec specs e2e vitest jest cypress playwright coverage testing'),
    ('mock', 'pencil', 0x8d6e63, 'mock mocks __mocks__ fixtures fixture stubs fakes factories'),
    ('jobs', 'clock', 0xffa726, 'jobs job cron crons workers worker queues queue schedulers scheduler'),
    ('tasks', 'check-list', 0x5c6bc0, 'tasks task todo todos'),
    ('temp', 'clock', 0x00897b, 'temp tmp .tmp cache .cache'),
    ('src', 'src', 0x66bb6a, 'src source sources'),
    ('app', 'app', 0xef5350, 'app apps'),
    ('lib', 'book', 0xafb42b, 'lib libs library libraries'),
    ('core', 'chip', 0x1e88e5, 'core kernel internal'),
    ('components', 'components', 0xc0ca33, 'components component ui widgets elements blocks'),
    ('features', 'puzzle', 0x7cb342, 'features feature modules module domains domain'),
    ('plugins', 'puzzle', 0x1e88e5, 'plugins plugin extensions extension addons'),
    ('middleware', 'puzzle', 0x7e57c2, 'middleware middlewares interceptors'),
    ('assets', 'image', 0xffb300, 'assets asset static res resources'),
    ('images', 'image', 0x26a69a, 'images image img imgs icons icon pictures photos screenshots brands logos media'),
    ('public', 'globe', 0x039be5, 'public www wwwroot web'),
    ('fonts', 'font', 0xe57373, 'fonts font typography'),
    ('audio', 'note', 0xef5350, 'audio sounds sound music'),
    ('video', 'play', 0xff9800, 'video videos movies'),
    ('styles', 'hash', 0x42a5f5, 'styles style css stylesheets theme themes'),
    ('sass', 'hash', 0xec407a, 'sass scss'),
    ('docs', 'lines', 0x1e88e5, 'docs doc documentation notes wiki guides manual'),
    ('templates', 'doc', 0x8d6e63, 'templates template partials snippets'),
    ('config', 'gear', 0x00acc1, 'config configs conf configuration settings .config env-config'),
    ('vscode', 'gear', 0x2196f3, '.vscode .idea .zed .fleet'),
    ('claude', 'spark', 0xd97757, '.claude claude agents prompts ai llm'),
    ('scripts', 'terminal', 0x78909c, 'scripts script bin cli commands command .husky'),
    ('tools', 'wrench', 0x1e88e5, 'tools tooling devtools'),
    ('utils', 'wrench', 0x7cb342, 'utils util utilities helpers helper shared common'),
    ('api', 'arrows', 0xffb300, 'api apis endpoints rest rpc'),
    ('routes', 'arrows', 0x43a047, 'routes route router routers routing'),
    ('controllers', 'arrows', 0xfbc02d, 'controllers controller handlers handler actions'),
    ('connectors', 'plug', 0xff7043, 'connectors connector integrations integration adapters adapter bridge bridges providers provider'),
    ('services', 'layers', 0xff7043, 'services service'),
    ('store', 'layers', 0xab47bc, 'store stores state redux context contexts reducers slices atoms'),
    ('db', 'cylinder', 0xffca28, 'db database databases migrations migration models model prisma schema schemas seeds seeders seed entities entity sql repositories repository'),
    ('data', 'braces', 0xfbc02d, 'data json datasets dataset content'),
    ('dist', 'box', 0xe57373, 'dist build builds out output target release releases .next .nuxt .output .svelte-kit .turbo'),
    ('packages', 'box', 0x7cb342, 'node_modules vendor packages package crates deps dependencies third_party'),
    ('hooks', 'hook', 0x7e57c2, 'hooks hook composables'),
    ('events', 'bell', 0xfdd835, 'events event listeners listener subscribers notifications'),
    ('i18n', 'globe', 0x5c6bc0, 'i18n l10n locales locale lang langs languages translations'),
    ('types', 'type', 0x1976d2, 'types type @types typings interfaces interface dto dtos'),
    ('git', 'branch', 0xff7043, '.git git'),
    ('github', 'branch', 0x78909c, '.github .gitlab'),
    ('ci', 'cycle', 0x1e88e5, 'ci .circleci workflows .buildkite pipelines'),
    ('pages', 'window', 0xff8a65, 'pages page views view screens screen routes-ui'),
    ('layouts', 'layout', 0x039be5, 'layouts layout'),
    ('client', 'monitor', 0x039be5, 'client clients frontend front-end web-app desktop'),
    ('server', 'server', 0xffb300, 'server servers backend back-end'),
    ('messages', 'chat', 0xff9800, 'messages message chat chats whatsapp conversations'),
    ('mail', 'mail', 0xef5350, 'mail mails email emails mailers mailer'),
    ('keys', 'key', 0x26a69a, 'keys key certs certificates ssh .ssh'),
    ('secure', 'lock', 0xffb300, 'secure security secrets secret private auth authentication'),
    ('guard', 'shield', 0x43a047, 'guards guard policies policy permissions'),
    ('debug', 'bug', 0xffb300, 'debug debugging'),
    ('errors', 'error', 0xef5350, 'errors error exceptions exception'),
    ('download', 'down', 0x43a047, 'download downloads'),
    ('upload', 'up', 0xff7043, 'upload uploads'),
    ('functions', 'func', 0x0288d1, 'functions function lambda lambdas'),
    ('serverless', 'bolt', 0x78909c, 'serverless .serverless edge'),
    ('docker', 'container', 0x0288d1, 'docker .docker containers container compose'),
    ('k8s', 'gear', 0x326ce5, 'kubernetes k8s helm charts'),
    ('infra', 'cloud', 0xffa000, 'infra infrastructure terraform deploy deployment deployments aws gcp azure cloud .aws'),
    ('examples', 'play', 0x00897b, 'examples example demo demos samples sample playground'),
    ('benchmark', 'gauge', 0x1e88e5, 'benchmark benchmarks bench perf'),
    ('archive', 'archive', 0x8d6e63, 'archive archives backup backups old legacy deprecated'),
    ('graphql', 'graph', 0xec407a, 'graphql gql'),
    ('reports', 'chart', 0x1e88e5, 'reports report analytics metrics stats'),
    ('env', 'leaf', 0x43a047, 'env envs environment environments'),
    ('logs', 'lines', 0xafb42b, 'logs log'),
    ('stories', 'book', 0xec407a, 'stories storybook .storybook'),
    ('constants', 'flag', 0x78909c, 'constants constant consts enums'),
    ('others', 'gear', 0x3fb950, ''),
]

def mix(c, other, t):
    r = [((c >> s) & 255) * (1 - t) + ((other >> s) & 255) * t for s in (16, 8, 0)]
    return (round(r[0]) << 16) | (round(r[1]) << 8) | round(r[2])

T = 'translate(8.7 8.7) scale(1.08)'

def glyph(parts, cut):
    o = ''
    for k, d in parts:
        if cut:
            fill = '#000' if k == 'f' else 'none'
            o += f'<path d="{d}" fill="{fill}" stroke="#000" stroke-width="{2.6 if k == "f" else 3.6}"/>'
        elif k == 'f':
            o += f'<path d="{d}" fill="#000"/>'
        else:
            o += f'<path d="{d}" fill="none" stroke="#000" stroke-width="1.3"/>'
    return f'<g transform="{T}" stroke-linecap="round" stroke-linejoin="round">{o}</g>'

def svg(body):
    return f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16">{body}</svg>\n'

def cut(body, parts):
    return svg(f'<defs><mask id="m"><rect width="16" height="16" fill="#fff"/>{glyph(parts, True)}</mask></defs><g mask="url(#m)">{body}</g>')

closed = f'<path d="{CLOSED}" fill="#000"/>'
opened = f'<path d="{OPEN_BACK}" fill="#000" fill-opacity="0.6"/><path d="{OPEN_FRONT}" fill="#000"/>'

here = os.path.dirname(os.path.abspath(__file__))
icons = os.path.join(here, 'icons')
for f in os.listdir(icons):
    if (f.startswith('folder-') or f.startswith('badge-')) and f not in ('folder-open.svg',):
        os.remove(os.path.join(icons, f))
def write(name, content):
    open(os.path.join(icons, name), 'w').write(content)
write('folder.svg', svg(closed))
write('folder-open.svg', svg(opened))

files, arms, seen = [], [], set()
for kind, g, color, names in KINDS:
    parts = GLYPHS[g]
    write(f'folder-{kind}.svg', cut(closed, parts))
    write(f'folder-{kind}-open.svg', cut(opened, parts))
    write(f'badge-{kind}.svg', svg(glyph(parts, False)))
    files += [f'folder-{kind}.svg', f'folder-{kind}-open.svg', f'badge-{kind}.svg']
    folder_c, badge_c = mix(color, 0x9aa5b1, 0.25), mix(color, 0xffffff, 0.42)
    ns = [n for n in names.split() if n not in seen]
    seen.update(ns)
    entry = f'Kind {{ closed: "icons/folder-{kind}.svg", open: "icons/folder-{kind}-open.svg", badge: "icons/badge-{kind}.svg", color: 0x{folder_c:06x}, badge_color: 0x{badge_c:06x} }}'
    if kind == 'others':
        arms.append(f'pub const OTHERS: Kind = {entry};')
        continue
    pats = ' | '.join(f'"{n}"' for n in ns)
    arms.append(f'        {pats} => {entry},')

consts = [a for a in arms if a.startswith('pub const')]
match = [a for a in arms if not a.startswith('pub const')]
rs = f'''//! Generated by assets/gen_folder_icons.py — do not edit by hand.

pub struct Kind {{
    pub closed: &'static str,
    pub open: &'static str,
    pub badge: &'static str,
    pub color: u32,
    pub badge_color: u32,
}}

{chr(10).join(consts)}

/// Folder kind for a lowercase folder name.
pub fn kind(lower: &str) -> Option<Kind> {{
    Some(match lower {{
{chr(10).join(match)}
        _ => return None,
    }})
}}

pub static FILES: &[(&str, &[u8])] = &[
{chr(10).join(f'    ("icons/{f}", include_bytes!("../assets/icons/{f}")),' for f in files)}
];
'''
open(os.path.join(here, '..', 'src', 'folder_icons.rs'), 'w').write(rs)
print(len(KINDS), 'kinds')
