# rename should support absolute path
root = (sh % "hg root").output
sh % "hg rebase -r $C -d $D '--config=ui.interactive=1' '--config=experimental.copytrace=off'" << f"""
{root}/Renamed
    path 'A' in commit 85b47c0eb942 was renamed to [what path relative to repo root] in commit ed4ad4ec6472 ? $TESTTMP/repo1/Renamed