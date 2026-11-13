"""Builds the made-up world the feature page's recordings show: projects (a git repository with
branches, a worktree, changes and ignored files), photos, downloads, and cm's settings.

    python -I tools/demo/make_world.py BASE [WORLD]

BASE/world is what the recordings show. record.ps1 maps it to a drive letter and passes that as
WORLD, so no real path appears, not even in what git keeps about the worktree. BASE/appdata
holds cm's settings, BASE/gitconfig git's. Photos and the big files are made once; everything
else is made again on every run, so each recording starts the same.
"""
import os
import random
import shutil
import subprocess
import sys
import time
import zipfile

from PIL import Image, ImageDraw

BASE = os.path.abspath(sys.argv[1])
WORLD = sys.argv[2] if len(sys.argv) > 2 else os.path.join(BASE, "world")
APPDATA = os.path.join(BASE, "appdata")
NOW = time.time()
DAY = 86400
REMOTE = "git@example.com:moon/website.git"


def write(path, text="", ago=None):
    path = os.path.join(WORLD, path)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(text)
    if ago is not None:
        stamp(path, ago)
    return path


def blob(path, size, head=b"", ago=None):
    path = os.path.join(WORLD, path)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    rng = random.Random(path)
    with open(path, "wb") as f:
        f.write(head)
        f.write(rng.randbytes(max(0, size - len(head))))
    if ago is not None:
        stamp(path, ago)


def stamp(path, ago):
    t = NOW - ago
    os.utime(path, (t, t))


def remove(path):
    def unlock(func, p, _):
        os.chmod(p, 0o666)
        func(p)

    if os.path.exists(path):
        shutil.rmtree(path, onexc=unlock)


GIT_ENV = dict(os.environ, GIT_CONFIG_GLOBAL=os.path.join(BASE, "gitconfig"), GIT_CONFIG_NOSYSTEM="1")


def git(repo, *args, ago=None):
    env = dict(GIT_ENV)
    if ago is not None:
        when = time.strftime("%Y-%m-%dT%H:%M:%S", time.localtime(NOW - ago))
        env.update(GIT_AUTHOR_DATE=when, GIT_COMMITTER_DATE=when)
    subprocess.run(["git", *args], cwd=os.path.join(WORLD, repo), env=env, check=True, capture_output=True)


def commit(repo, message, ago, files):
    for path, text in files.items():
        write(os.path.join(repo, path), text)
    git(repo, "add", "-A")
    git(repo, "commit", "-q", "-m", message, ago=ago)


# --- the website: a repository with history, branches, a worktree, changes, ignored files

INDEX = """<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>Moonlight Studio</title>
  <link rel="stylesheet" href="style.css">
</head>
<body>
  <site-header></site-header>
  <main id="gallery"></main>
  <site-footer></site-footer>
  <script type="module" src="app.js"></script>
</body>
</html>
"""
APP = """import { Header } from "./components/header.js";
import { Footer } from "./components/footer.js";
import { Gallery } from "./components/gallery.js";

customElements.define("site-header", Header);
customElements.define("site-footer", Footer);

const gallery = new Gallery(document.querySelector("#gallery"));
gallery.load("/api/photos?limit=24");
"""
STYLE = """:root {
  --night: #101828;
  --moon: #f4e9c8;
  --accent: #7aa2f7;
}

body {
  margin: 0;
  font-family: system-ui, sans-serif;
  background: var(--night);
  color: var(--moon);
}

#gallery {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(220px, 1fr));
  gap: 12px;
}
"""
HEADER = """export class Header extends HTMLElement {
  connectedCallback() {
    this.innerHTML = `
      <header class="site-header">
        <a href="/" class="logo"><img src="/assets/logo.svg" alt=""></a>
        <nav><a href="/about.html">About</a> <a href="/contact.html">Contact</a></nav>
      </header>`;
  }
}
"""
FOOTER = """export class Footer extends HTMLElement {
  connectedCallback() {
    this.innerHTML = `<footer>Moonlight Studio, ${new Date().getFullYear()}</footer>`;
  }
}
"""
GALLERY = """export class Gallery {
  constructor(element) {
    this.element = element;
  }

  async load(url) {
    const photos = await (await fetch(url)).json();
    for (const photo of photos) {
      const img = document.createElement("img");
      img.loading = "lazy";
      img.src = photo.thumbnail;
      img.alt = photo.title;
      this.element.append(img);
    }
  }
}
"""
SEARCH = """export class Search extends HTMLElement {
  connectedCallback() {
    this.innerHTML = `<input type="search" placeholder="Search photos">`;
    this.querySelector("input").addEventListener("input", (e) => this.find(e.target.value));
  }

  find(text) {
    this.dispatchEvent(new CustomEvent("search", { detail: text.trim().toLowerCase() }));
  }
}
"""


def website():
    repo = "projects/website"
    os.makedirs(os.path.join(WORLD, repo))
    git(repo, "init", "-q", "-b", "main")
    commit(repo, "Start the website: index and styles", 30 * DAY, {
        "src/index.html": INDEX,
        "src/style.css": STYLE,
        "README.md": "# Moonlight Studio\n\nThe studio's website: a gallery of night photography.\n\n"
                     "    npm install\n    npm run dev\n\nSee docs/setup.md and docs/deploy.md.\n",
        "package.json": '{\n  "name": "moonlight-studio",\n  "version": "2.0.0",\n  "scripts": {\n'
                        '    "dev": "vite",\n    "build": "vite build"\n  },\n  "devDependencies": {\n'
                        '    "vite": "^7.1.0"\n  }\n}\n',
        ".gitignore": "node_modules/\ndist/\n*.log\n",
    })
    commit(repo, "Header and footer components", 26 * DAY, {
        "src/components/header.js": HEADER,
        "src/components/footer.js": FOOTER.replace("${new Date().getFullYear()}", "2025"),
        "src/app.js": APP.replace('import { Gallery } from "./components/gallery.js";\n', "")
                         .replace('\nconst gallery = new Gallery(document.querySelector("#gallery"));\n'
                                  'gallery.load("/api/photos?limit=24");\n', ""),
        "assets/logo.svg": '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">'
                           '<path d="M40 8a24 24 0 1 0 16 40A20 20 0 0 1 40 8z" fill="#f4e9c8"/></svg>\n',
    })
    commit(repo, "About and contact pages", 21 * DAY, {
        "src/pages/about.html": "<h1>About</h1>\n<p>We photograph cities after dark.</p>\n",
        "src/pages/contact.html": '<h1>Contact</h1>\n<form id="contact">\n  <input name="email" type="email">\n'
                                  '  <textarea name="message"></textarea>\n  <button>Send</button>\n</form>\n',
    })
    commit(repo, "Gallery on the front page", 17 * DAY, {
        "src/components/gallery.js": GALLERY.replace('      img.loading = "lazy";\n', ""),
        "src/app.js": APP,
    })
    commit(repo, "Docs: setup and deploy", 12 * DAY, {
        "docs/setup.md": "# Setup\n\n1. Install Node 22.\n2. `npm install`\n3. `npm run dev`, then open http://localhost:5173\n",
        "docs/deploy.md": "# Deploy\n\n`npm run build` makes `dist/`; the pipeline copies it to the server.\n",
    })
    git(repo, "branch", "release/2.0")
    commit(repo, "Lazy-load gallery images", 9 * DAY, {"src/components/gallery.js": GALLERY})
    git(repo, "branch", "feature/search")
    commit(repo, "Footer links and year", 6 * DAY, {"src/components/footer.js": FOOTER})
    for branch, message, ago, files in (
        ("fix/header-spacing", "Header: even spacing on narrow screens", 2 * DAY,
         {"src/components/header.js": HEADER.replace('class="site-header"', 'class="site-header compact"')}),
        ("docs/deploy-guide", "Deploy guide: how to roll back", 5 * DAY,
         {"docs/deploy.md": "# Deploy\n\n`npm run build` makes `dist/`.\n\n## Rolling back\n\nRun the previous pipeline again.\n"}),
    ):
        git(repo, "switch", "-q", "-c", branch)
        commit(repo, message, ago, files)
        git(repo, "switch", "-q", "main")
    git(repo, "remote", "add", "origin", REMOTE)
    git(repo, "update-ref", "refs/remotes/origin/main", "HEAD")
    git(repo, "config", "branch.main.remote", "origin")
    git(repo, "config", "branch.main.merge", "refs/heads/main")
    # A branch only the remote has.
    git(repo, "switch", "-q", "-c", "dark-mode")
    commit(repo, "Dark mode toggle", 20 * 3600, {"src/style.css": STYLE + "\n@media (prefers-color-scheme: light) {\n  body { background: #fff; }\n}\n"})
    git(repo, "update-ref", "refs/remotes/origin/feature/dark-mode", "HEAD")
    git(repo, "switch", "-q", "main")
    git(repo, "branch", "-q", "-D", "dark-mode")
    commit(repo, "Contact form: check the address", 3 * 3600, {
        "src/pages/contact.html": '<h1>Contact</h1>\n<form id="contact">\n  <input name="email" type="email" required>\n'
                                  '  <textarea name="message" required></textarea>\n  <button>Send</button>\n</form>\n',
    })
    # The search branch, two commits ahead, checked out in a worktree of its own.
    git(repo, "switch", "-q", "feature/search")
    commit(repo, "Search box in the header", 4 * DAY, {"src/components/search.js": SEARCH})
    commit(repo, "Search: highlight what matches", 26 * 3600,
           {"src/components/search.js": SEARCH.replace("find(text) {", "find(text) {\n    this.highlight(text);")})
    git(repo, "switch", "-q", "main")
    git(repo, "worktree", "add", "-q", "../website-search", "feature/search")
    # git writes the worktree's links with the real path behind a subst drive: point them at
    # the world's own, which is all the recordings may show.
    main_git = os.path.join(WORLD, repo, ".git").replace("\\", "/")
    linked = os.path.join(WORLD, "projects/website-search").replace("\\", "/")
    for path, text in ((f"{main_git}/worktrees/website-search/gitdir", f"{linked}/.git\n"),
                       (f"{linked}/.git", f"gitdir: {main_git}/worktrees/website-search\n")):
        # In place: git hides .git, and Windows won't replace a hidden file.
        with open(path, "r+", encoding="utf-8", newline="\n") as f:
            f.write(text)
            f.truncate()
    write("projects/website-search/src/components/search.js",
          SEARCH.replace("Search photos", "Search photos and places"), ago=40 * 60)
    # Work under way on main: three changed files, one new, and what git ignores.
    write(f"{repo}/src/app.js", APP + '\ndocument.title += " (beta)";\n', ago=25 * 60)
    write(f"{repo}/src/style.css", STYLE.replace("gap: 12px;", "gap: 16px;"), ago=18 * 60)
    write(f"{repo}/docs/setup.md", "# Setup\n\n1. Install Node 22 or newer.\n2. `npm install`\n"
                                   "3. `npm run dev`, then open http://localhost:5173\n", ago=50 * 60)
    write(f"{repo}/notes.txt", "- search: highlight in the gallery too\n- dark mode: remember the choice\n", ago=8 * 60)
    for package in ("vite", "esbuild", "rollup", "postcss", "nanoid"):
        write(f"{repo}/node_modules/{package}/package.json", f'{{"name": "{package}"}}\n', ago=12 * DAY)
    write(f"{repo}/dist/index.html", INDEX, ago=2 * DAY)
    write(f"{repo}/build.log", "vite v7.1.0 building for production...\n✓ 14 modules transformed.\n", ago=2 * DAY)


def other_projects():
    write("projects/api/main.py", 'from fastapi import FastAPI\n\napp = FastAPI()\n\n\n@app.get("/photos")\n'
                                  'def photos(limit: int = 24):\n    return []\n', ago=8 * DAY)
    write("projects/api/requirements.txt", "fastapi\nuvicorn\n", ago=8 * DAY)
    write("projects/api/README.md", "# Photo API\n", ago=8 * DAY)
    for name, ago in (("ideas.md", 3), ("reading list.md", 14), ("trip plans.md", 40), ("shopping.md", 90)):
        write(f"projects/notes/{name}", f"# {name[:-3].capitalize()}\n", ago=ago * DAY)


def photos():
    for folder, names, start in (
        ("photos/2024 Iceland", [f"IMG_{4100 + i}.jpg" for i in range(1, 25)], 850),
        ("photos/2025 Prague", [f"IMG_{5200 + i}.jpg" for i in range(1, 13)] + [f"IMG_{5300 + i}.CR3" for i in range(1, 4)], 550),
        ("photos/2025 Lisbon", [f"IMG_{6000 + i}.jpg" for i in range(1, 11)] + ["tram 28.mp4"], 420),
    ):
        os.makedirs(os.path.join(WORLD, folder), exist_ok=True)
        for i, name in enumerate(names):
            path = os.path.join(WORLD, folder, name)
            if not os.path.exists(path):
                rng = random.Random(name)
                if name.endswith(".jpg"):
                    w, h = rng.choice([(4000, 3000), (3000, 4000), (4000, 2250)])
                    noise = Image.effect_noise((w // 2, h // 2), 60).resize((w, h))
                    sky = Image.linear_gradient("L").resize((w, h))
                    tint = (rng.randrange(20, 90), rng.randrange(30, 110), rng.randrange(80, 200))
                    image = Image.merge("RGB", (sky, noise, sky)).point(lambda v: v * 0.6)
                    ImageDraw.Draw(image).rectangle([0, h * 2 // 3, w, h], fill=tint)
                    image.save(path, quality=80)
                elif name.endswith(".CR3"):
                    blob(os.path.relpath(path, WORLD), 28_000_000 + rng.randrange(4_000_000), b"\x00\x00\x00\x18ftypcrx ")
                else:
                    blob(os.path.relpath(path, WORLD), 64_000_000, b"\x00\x00\x00\x20ftypisom")
            stamp(path, start * DAY - i * 3600)


def downloads():
    remove(os.path.join(WORLD, "downloads"))
    blob("downloads/report 2025.pdf", 1_240_000, b"%PDF-1.7\n", ago=6 * DAY)
    blob("downloads/setup-tool.exe", 3_400_000, b"MZ\x90\x00", ago=20 * DAY)
    blob("downloads/podcast 12.mp3", 41_000_000, b"ID3\x04\x00", ago=3 * DAY)
    write("downloads/readme.txt", "Thanks for downloading!\n", ago=20 * DAY)
    write("downloads/draft.txt.bak", "old draft\n", ago=45 * DAY)
    # A zip of the website as it was at the release.
    path = os.path.join(WORLD, "downloads/website-backup.zip")
    site = os.path.join(WORLD, "projects/website")
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        for folder, dirs, files in os.walk(site):
            dirs[:] = [d for d in dirs if d not in (".git", "node_modules", "dist")]
            for name in files:
                full = os.path.join(folder, name)
                z.write(full, os.path.join("website", os.path.relpath(full, site)))
    stamp(path, 9 * DAY)
    # The big file a copy shows its speed with: its length only, so it costs nothing to make.
    big = os.path.join(WORLD, "videos/night timelapse.mkv")
    if not os.path.exists(big):
        os.makedirs(os.path.dirname(big), exist_ok=True)
        with open(big, "wb") as f:
            f.truncate(6 * 1024 ** 3)
    stamp(big, 33 * DAY)
    blob("videos/aurora.mp4", 180_000_000, b"\x00\x00\x00\x20ftypisom", ago=470 * DAY) if not os.path.exists(
        os.path.join(WORLD, "videos/aurora.mp4")) else None


def settings():
    remove(APPDATA)
    folder = os.path.join(APPDATA, "crescent-moon")
    os.makedirs(folder)
    files = {
        "settings.ini": "confirm_quit = false\nscheme = classic-v2\nprompt_host = false\n",
        "hotlist.txt": "M:\\projects\\website\nM:\\photos\nM:\\downloads\nM:\\projects\\notes\n",
        "places.ini": f"; Ctrl+E: places kept per git repository\n[{REMOTE}]\n"
                      "c = Components | src/components\np = Pages | src/pages\nd = Docs | docs\n"
                      "s = Styles | src/style.css\n",
    }
    for name, text in files.items():
        with open(os.path.join(folder, name), "w", encoding="utf-8", newline="\n") as f:
            f.write(text)


def folder_times():
    """Each folder as old as the newest thing in it, as if it had grown there."""
    for folder, dirs, files in os.walk(WORLD, topdown=False):
        if ".git" in folder.split(os.sep):
            continue
        times = [os.path.getmtime(os.path.join(folder, n)) for n in dirs + files if n != ".git"]
        if times and os.path.abspath(folder) != os.path.abspath(WORLD):
            os.utime(folder, (max(times), max(times)))


def main():
    with open(os.path.join(BASE, "gitconfig"), "w", encoding="utf-8") as f:
        f.write("[user]\n\tname = Moon Demo\n\temail = demo@example.com\n[init]\n\tdefaultBranch = main\n"
                "[core]\n\tautocrlf = false\n\tpager = cat\n[advice]\n\tdetachedHead = false\n")
    for folder in ("projects", "backup"):
        remove(os.path.join(WORLD, folder))
    website()
    other_projects()
    photos()
    downloads()
    os.makedirs(os.path.join(WORLD, "backup"))
    settings()
    folder_times()
    print("world ready:", WORLD)


if __name__ == "__main__":
    main()
