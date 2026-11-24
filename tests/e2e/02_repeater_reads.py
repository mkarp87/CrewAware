import json,sys,time,urllib.request
from playwright.sync_api import sync_playwright
BASE="http://127.0.0.1:18082"
errs=[]
def state(): return json.load(urllib.request.urlopen(BASE+"/api/state"))
def check(c,msg):
    print(("PASS " if c else "FAIL ")+msg)
    if not c: errs.append("FAIL "+msg)
with sync_playwright() as p:
    b=p.chromium.launch(executable_path="/usr/bin/google-chrome",args=["--no-sandbox"])
    pg=b.new_page(viewport={"width":1400,"height":850})
    pg.on("pageerror",lambda e:errs.append("pageerror: "+str(e)))
    pg.on("console",lambda m:errs.append("console: "+m.text) if m.type=="error" else None)
    pg.goto(BASE); pg.wait_for_selector("#tabs button"); pg.fill("#seat","Tester")
    pg.click("#tabs >> text=Setup")
    if pg.locator("#lu").count():
        pg.fill("#lu","admin"); pg.fill("#lp","adminpass1234"); pg.click("#btnLogin"); pg.wait_for_selector("text=Signed in as admin")
    # passwords through the UI
    pg.click(".subtabs >> text=Crew"); pg.wait_for_selector("text=Repeater passwords")
    for name in ["Rpt North","Rpt East","Rpt West"]:
        pg.select_option("#credRep",label=name) if False else None
        opts=pg.locator("#credRep option").all_inner_texts()
        lab=[o for o in opts if o.startswith(name)][0]
        pg.select_option("#credRep",label=lab)
        pg.fill("#credPw","demo-password"); pg.click("text=Save password"); pg.wait_for_timeout(700)
    s=state(); check(all(r['has_credentials'] for r in s['repeaters']),"passwords stored")
    # wait for the poll
    for _ in range(40):
        s=state()
        if any(r.get('reading') for r in s['repeaters']): break
        time.sleep(1)
    time.sleep(6); s=state()
    for r in s['repeaters']: print(r['name'],r['contact'],r.get('reading'))
    check(any(r.get('reading') and r['reading']['ok'] for r in s['repeaters']),"repeater read through the radio")
    pg.click("#tabs >> text=Units"); pg.wait_for_timeout(500)
    txt=pg.inner_text("#body"); print(txt[:900])
    check("V" in txt and "noise" in txt,"reading text shown")
    pg.screenshot(path="/tmp/e2e/s2.png")
    print("errors",errs); b.close()
