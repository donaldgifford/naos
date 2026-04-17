# cliff.toml — git-cliff configuration
# https://git-cliff.org/docs/configuration

[changelog]
header = """
# Changelog

All notable changes to this project will be documented in this file.

"""
body = """
{%- macro remote_url() -%}
  https://github.com/donaldgifford/rondo
{%- endmacro -%}

{% if version -%}
  ## [{{ version | trim_start_matches(pat="v") }}] - {{ timestamp | date(format="%Y-%m-%d") }}
{% else -%}
  ## [Unreleased]
{% endif -%}

{% for group, commits in commits | group_by(attribute="group") %}
  ### {{ group | striptags | trim | upper_first }}
  {% for commit in commits %}
    - {% if commit.scope %}**{{ commit.scope }}**: {% endif %}{{ commit.message | upper_first }}\
  {% endfor %}
{% endfor %}\n
"""
footer = """
{% for release in releases -%}
  {% if release.version -%}
    {% if release.previous.version -%}
      [{{ release.version | trim_start_matches(pat="v") }}]: \
        https://github.com/donaldgifford/rondo/compare/{{ release.previous.version }}...{{ release.version }}
    {% endif -%}
  {% else -%}
    {% if release.previous.version -%}
      [Unreleased]: https://github.com/donaldgifford/rondo/compare/{{ release.previous.version }}...HEAD
    {% endif -%}
  {% endif -%}
{% endfor %}
"""
trim = true

[git]
conventional_commits = true
filter_unconventional = true
split_commits = false
commit_parsers = [
  { message = "^feat", group = "Added" },
  { message = "^fix", group = "Fixed" },
  { message = "^perf", group = "Performance" },
  { message = "^refactor", group = "Changed" },
  { message = "^docs", group = "Documentation" },
  { message = "^test", group = "Testing" },
  { message = "^ci", group = "CI" },
  { message = "^chore\\(release\\)", skip = true },
  { message = "^chore", group = "Miscellaneous" },
]
protect_breaking_commits = false
filter_commits = false
tag_pattern = "v[0-9].*"
sort_commits = "oldest"
