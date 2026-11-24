import json,sys,urllib.request
from playwright.sync_api import sync_playwright
BASE="http://127.0.0.1:18082"
errs=[]
def state(): return json.load(urllib.request.urlopen(BASE+"/api/state"))
with sync_playwright() as p:
    b=p.chromium.launch(executable_path="/usr/bin/google-chrome",args=["--no-sandbox"])
    pg=b.new_page(viewport={"width":1400,"height":850})
    pg.on("pageerror",lambda e:errs.append("pageerror: "+str(e)))
    pg.on("console",lambda m:errs.append("console: "+m.text) if m.type=="error" else None)
    pg.goto(BASE); pg.wait_for_selector("#tabs button")
    pg.fill("#seat","Tester")
    # login
    pg.click("#tabs >> text=Setup")
    pg.fill("#lu","admin"); pg.fill("#lp","adminpass1234"); pg.click("#btnLogin")
    pg.wait_for_selector("text=Signed in as admin")
    print("logged in")
    # site: overlay upload
    pg.set_input_files("input[type=file] >> nth=1","/tmp/e2e/overlay.png")
    pg.wait_for_function("document.querySelector('#map image')!==null",timeout=8000)
    print("overlay shown", state()['site']['img_w'],state()['site']['img_h'])
    pg.fill("#siteNameIn","Demo Ranch"); pg.fill("#sloganIn","Know where your crew is")
    for k,v in dict(apx=0,apy=0,alat=39.0030,alon=-104.0035,bpx=1000,bpy=700,blat=38.9985,blon=-103.9960).items(): pg.fill("#"+k,str(v))
    pg.click("#btnSaveSite"); pg.wait_for_function("document.getElementById('siteName').textContent==='Demo Ranch'")
    print("site saved", pg.inner_text("#slogan"))
    # radio: adopt all
    import time
    for _ in range(40):
        if len(state()['radio']['discovered'])+len(state()['units'])+len(state()['repeaters'])>=9: break
        time.sleep(0.5)
    pg.click(".subtabs >> text=Radio"); pg.wait_for_selector("text=Heard on the mesh")
    n=0
    while pg.locator("#body button:has-text('Adopt')").count():
        pg.locator("#body button:has-text('Adopt')").first.click(); pg.wait_for_timeout(500); n+=1
        if n>12: break
    print("adopted",n)
    s=state(); print([ (u['name'],u['has_pos'],u['node_key'][:6]) for u in s['units']]); print([(r['name'],r['placed'],r['has_credentials']) for r in s['repeaters']])
    pg.screenshot(path="/tmp/e2e/s1.png")
    print("errors",errs)
    b.close()
