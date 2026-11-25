# ChatOid 0.1.2

ChatOid, insanların ve yerel Codex ajanlarının bire bir ve grup sohbeti yapması için iki masaüstü istemcisi sunar. Bire bir sohbetin hostu isteği açan cihazdır. Grubun hostu grup yöneticisinin cihazıdır. Host, sohbet geçmişini SQLite’ta tutar. Cihazlar aynı yerel ağ veya VPN üzerinde HTTPS ile bağlanır.

## Başlangıç

1. Windows’ta `chatoid.exe`, Linux’ta `chatoid` ile insan uygulamasını aç. **Hesap oluştur** bölümünde benzersiz bir kullanıcı adı, görünen ad ve en az 6 karakterlik bir parola seç.
2. Sohbet başlatacak cihazda **Host** sayfasından yerel ağ veya VPN arayüzünü seç. Adresi ve portu elle de girebilirsin. Örnek `192.168.1.20:47821` veya `100.78.12.9:47821`. **Bu cihazı host yap** seçeneğini kullan. Ağdaki diğer cihazların bu porta erişebilmesi gerekir. Varsayılan `127.0.0.1` adresi aynı cihazda deneme içindir.
3. Karşı taraf **Kişiler ve istekler → Kişi kartımı kopyala** ile kartını paylaşır. Kodu içe aktar, kişiyi kullanıcı adıyla bul ve **Arkadaşlık isteği gönder** seçeneğini kullan. Panoya kopyalanan davet kodunu karşı tarafa ilet.
4. Aynı cihazda ChatOid’e giriş yapmış bir alıcının isteği **Kişiler ve istekler** bölümüne doğrudan gelir. Başka bir cihazdaki alıcı, aynı ekranda davet kodunu içe aktarır. Alıcı kabul edince bire bir sohbet iki tarafta da **Sohbetler** listesine eklenir.
5. **Yeni grup** ile kendi cihazında bir grup kur. ChatOid grubun hostu olarak kurucu hesabını kaydeder ve sohbet başlığında gösterir. Grubun **Davet et** menüsünde arkadaşlığını kabul eden kişileri seç. Davet bire bir sohbete düşer. Panodaki davet kodunu da paylaşabilirsin. Davetli kendi kabulünden sonraki mesajları görür.

Bir mesaja **Yanıtla** ile alıntı ekleyebilirsin. Alıntıya tıklamak kaynak mesaja gider. Arşivlenen sohbet yeni mesaj geldiğinde arşivde kalır. Host çevrimdışıyken sıraya alınan mesajlar **Bekliyor** görünür ve bağlantı geri geldiğinde yeniden gönderilir.

Grup sohbetinde `@` yazarak bir üyeyi etiketleyebilir, bir avatara veya etikete tıklayarak kişi profilini açabilirsin. **Profilim** ekranında görünen ad ve fotoğraf değiştirilebilir. Ajan kaydında hazır ad ve avatar önerileri bulunur. Ajanın **Gizlilik** ayarında özetlenen cihaz bilgileri varsayılan olarak kabul edilmiş arkadaşlara görünür. Sohbetin sağ tık menüsündeki **Mesaj bilgisi** gönderim saatini ve alıcıların teslim veya okuma durumunu gösterir.

Windows’ta ataç, sürükle-bırak veya mesaj alanında `Ctrl+V` ile ek seçebilirsin. Güncel hostlarda sınır 32 MiB, eski hostlarda uygulamanın gösterdiği sınır geçerlidir. Ekler önce taslakta görünür. JPG ve PNG sohbet içinde önizlenir. Taslaklar hesap ve sohbet başına saklanır. **Ayarlar → Görünüm** ile bir arka plan seçebilir ve seçili sohbete özel arka plan belirleyebilirsin.

**Kişiler ve istekler → Bağlantımı paylaş** ile tek bir imzalı bağlantı paylaşabilirsin. Alıcıya gidecek istek, alıcının hostu çevrimdışıyken kuyrukta kalır. Aynı cihazdaki profiller, gizlilik ayarında izin verirlerse kişi ekleme ekranında görünür. **Arkadaşlıktan çıkar** işlemi bire bir sohbeti iki tarafta salt okunur yapar ve geçmişi saklar. Ayrı bir hosttaki grup daveti ve kabulü, ilgili arkadaşlık hostundan güncel doğrulama bekler.

Pencereyi kapatma düğmesi ve **Uygulamadan çık** istemciyi ve bu istemcinin çalıştırdığı hostu kapatır. Host açık kalsın istiyorsan pencereyi küçült. **Hostu durdur** ve **Oturumu kapat** da hostu durdurur. Host durduğunda uzaktaki üyeler önbellekteki sohbeti okuyabilir. Adres değiştiğinde aynı hostu yeni adresle başlat ve **Host bağlantısını kopyala** ile yeni kodu mevcut üyelere paylaş. Üyeler kodu **Kişiler ve istekler** ekranında içe aktarabilir.

Yerel sohbet listesi boşsa ve host eski kayıtları hâlâ tutuyorsa, aynı hesabınla host bağlantısını içe aktararak arkadaşlıkları ve erişebildiğin sohbetleri yeniden eşitleyebilirsin. Bu hosta ilk kez erişecek bir hesabın davet koduna ihtiyacı vardır.

Cihazdaki her ChatOid hesabı arkadaşlarını, isteklerini, sohbet önbelleğini, arşivini ve bekleyen mesajlarını kendi profiline kaydeder. Uygulama güncellendiğinde aynı cihazda, aynı hesapla ve aynı veri klasörüyle aç.

Giriş ekranındaki **Beni hatırla** seçeneği, sonraki açılışta bu cihazda son seçilen insan hesabını açar. **Oturumu kapat** otomatik girişi iptal eder. Windows oturum anahtarını DPAPI ile korur. Linux'ta otomatik giriş için çalışan bir Secret Service ve `secret-tool` (`libsecret-tools`) gerekir. Bu bileşenler yoksa parola ile giriş yapabilirsin.

## Ajan kurma

1. Ajan cihazında Codex CLI ile oturum aç. `chatoid-agent.exe` veya `chatoid-agent` ile ayrı ajan uygulamasını aç. **Ajan oluştur** ile kullanıcı adı, görünen ad, model adı ve görevi gir. Alt ajan oluşturuyorsan üst ajanının adını da ekle. Bu bilgiler yeni `.key` dosyasına kaydedilir. Mevcut anahtarı açmak için **Anahtarı aç** seçeneğini kullan.
2. **Ajan** sayfasında Codex’in çalıştırılabilir dosya yolunu gir. Gerekirse **OpenAI ile giriş yap** düğmesiyle tarayıcıda Codex oturumunu aç. **Deneme yanıtı al** ile bağlantıyı kontrol et. **Yeni sohbet mesajlarını değerlendir** seçeneğini aç ve ayarları kaydet.
3. İnsan operatörünün kişi kartını **Operatörü eşleştir** alanına gir. Ajan bir istek alınca **Operatör bağlantısını kopyala** ile o isteğe özel yetki bağlantısı oluşur. Operatör bunu kendi insan uygulamasına aktarır ve isteği oradan kabul eder veya reddeder.
4. Otomatik karar için arkadaşlık veya grup iznini ajan ayarlarından aç ve karar kurallarını yaz. Ajan ile operatör aynı anda karar verirse host ilk kaydı korur.
5. Ajan üye olduktan sonra salt okunur bir klasör seç. **Çalışma klasörü** bölümünde hangi sohbetten hangi kişilerin dosya incelemesi isteyebileceğini işaretle ve kaydet. Grupta `@poyraz`, alıntılı yanıt veya «Poyraz, README.md dosyasını incele» gibi doğal bir hitap kullan.

### Codex görevleri ve pencere araçları

Yerel Codex oturumu zaten açıksa normal istekler bu bağlantıyı geçici bir Codex çalıştırmasında kullanır; kimlik bilgileri kopyalanmaz ve Codex masaüstüne yeni sohbet eklenmez. İstersen **Görevler için OpenAI ile giriş yap** ile ayrı ve kalıcı bir görev oturumu bağlayabilirsin.

Operatörü mevcut insan arkadaşların arasından eşleştir ve Codex çalışma görevlerini etkinleştir. Eşleşmiş operatörün bire bir mesajları varsayılan olarak doğrudan çalışır. Klasör seçilmezse ajana özel yerel çalışma alanı kullanılır. Masaüstü görüntüsü göndermesi açıkça istendiğinde bu tek çekim ve aynı sohbete gönderim ayrıca onay beklemez; diğer pencere eylemleri ve dosya paylaşımları kendi onaylarını korur. İstersen **Her mesaj için ayrıca çalıştırma onayı iste** seçeneğini açabilirsin.

**Görev süre sınırı** yeni başlatılacak Codex işinin en uzun çalışma süresini belirler. Varsayılan 15 dakikadır. Ajan ayarlarında 1–1440 dakika seçebilirsin.

Görev onayında **Codex modellerini yükle** seçeneği, kurulu Codex’in görev profiline sunduğu modelleri getirir. **Görev modeli** ve **Düşünme düzeyi** alanlarından o görev için seçim yapabilirsin. Varsayılan seçenek yeni görevde Codex ayarını, devam eden görevde mevcut oturum ayarını kullanır.

**Codex becerileri → Becerileri yükle** ile seçili çalışma klasörü ve ajan profili için etkin becerileri görebilirsin. İstediğin becerileri işaretleyip **Planla** veya **Bu klasörde çalıştır** seçeneğine bas. Seçtiğin beceriler o görev adımına eklenir.

Kurulu Codex'in kişisel, sistem ve eklenti skill klasörleri otomatik bulunur ve yerinde kullanılır; kopyalamak gerekmez. **Ek skill klasörleri → Skill klasörü ekle** ile başka kaynaklar tanıtılabilir. Görev panelinde beceri listesini yenile. Projedeki uygulanabilir `AGENTS.md` ve skill talimatları, onaylanan görevin ve izinlerin sınırları içinde uygulanır. Skill erişimi, gerektirdiği harici servisin bağlantısını veya oturumunu sağlamaz; bu bağlantılar ayrıca kurulmalıdır.

Operatör “Masaüstünün ekran görüntüsünü at” istediğinde Codex, pencere seçmeden tüm bağlı ekranları tek seferde yakalayabilen `chatoid_desktop_image` aracını kullanır. Yerel eylem onayından sonra görüntü görev yanıtına eklenir. Kalıcı yakalama oturumu açılmaz; görüntü kaynakları çekimden hemen sonra serbest bırakılır.

Görev panelindeki **MCP bağlantıları** bölümünde ajan profilindeki Codex’e tanımlı sunucuları ve sundukları araçları listeleyebilirsin. **Bu görevde listedeki MCP araçlarını kullan** seçeneğiyle çalıştırma görevine araç erişimi verebilirsin. Her çağrıda sunucu, araç adı ve JSON parametreleri ajan panelinde onaya sunulur. MCP sunucuları kendi yapılandırılmış yetkileriyle çalışır.

**Yeni MCP bağlantısı** formunda bir sunucu adresi veya yerel program tanımlayabilirsin. Yerel program argümanlarını her satıra bir tane yaz. **Bağlantıyı kaydet**, yalnızca ajanın özel Codex profiline kaydeder; mevcut aynı adlı bağlantıyı değiştirmez. Ardından **Bağlantıları ve araçları listele** ile bağlan. Yerel program bu adımda başlatılır. Diğer Codex ayarları korunur.

OAuth destekleyen MCP bağlantısında **Tarayıcıda giriş yap** düğmesi Codex’in hizmete giriş akışını açar. Giriş başarılı olduğunda bağlantı listesi yenilenir. Kimlik bilgileri ajanın ayrı Codex görev profiline kaydedilir.

MCP araçları doğrudan görevin Codex oturumunda çalışır. Çok adımlı araçların bağlantısı görev boyunca korunur; sonuçları ve görselleri Codex'in kendi MCP işleyicisi modele iletir. ChatOid, Codex'in araç onaylarını gösterir. Görev bittiğinde veya durdurulduğunda bu oturumun yerel MCP süreçleri kapatılır.

Görev onayında **Planla** seçeneği, Codex’in çalışma klasörünü salt okunur inceleyip sohbete bir plan göndermesini sağlar. Planı uygulamak için plan yanıtını alıntılayıp devam isteği gönder ve ajan uygulamasında **Bu klasörde çalıştır** seçeneğini kullan.

Çalışma klasörü bir Git deposuysa, görev onayındaki **Kod incelemesi** bölümünden kaydedilmemiş değişiklikleri, bir temel dalla farkları veya bir commit’i seçebilirsin. **İncelemeyi başlat** salt okunur Codex incelemesi yapar ve bulguları sohbete gönderir.

Normal mesajlar aynı sohbetin son görünür mesajlarıyla değerlendirilir. Ayrı görev oturumunda oluşturulan bir yanıtı alıntılarsan aynı yerel Codex geçmişiyle devam edilebilir. Geçici çalıştırmaların sohbet bağlamı ChatOid kayıtlarından alınır.

Uzun bir göreve devam ederken **Önce Codex bağlamını sıkıştır** seçeneğini kullanabilirsin. Codex önceki bağlamı özetler ve yeni isteği aynı görev oturumunda çalıştırır.

Codex kullanım verisi bildirdiğinde görev panelindeki **Token kullanımı** bölümünde oturum toplamı, girdi, önbellekten okunan girdi, çıktı ve düşünme tokenları görünür. Son model çağrısının kullanımı ayrıca gösterilir.

Bir görev yanıtını alıntılayıp **Ayrı bir kolda devam et** seçeneğini işaretlersen Codex, o yanıta kadarki geçmişi yeni bir oturuma kopyalar. Kolların her birinde ilerlemek için o kolun yanıtını alıntıla. Dallandırma, Codex turu kayıtlı görev yanıtlarında kullanılabilir.

Görev sürerken başlangıç mesajını alıntıla ve `/yönlendir` ile yeni talimatı yaz. Başlangıç mesajının sağ tık menüsündeki **Görevi yönlendir** bunu hazırlar. Çalışan görevi eşleşmiş operatör yönlendirebilir. Codex talimatı kabul edince sohbet, yönlendirme mesajına alıntılı bir bildirim ekler. Pencere ve dosya paylaşımı onayları geçerliliğini korur.

Başlangıç mesajının menüsünden **Görev durumu** ile ilerlemeyi sorabilir, **Görevi durdur** ile çalışan görevi durdurabilir veya henüz onaylanmamış görevi iptal edebilirsin. Başlangıç mesajını alıntılayarak `/durum` ve `/durdur` komutlarını da gönderebilirsin.

Kesilen veya durdurulan görev için **Yeniden onaya al** seçeneğiyle aynı isteği tekrar hazırlayabilirsin. Model ve izinleri yeniden seçip görevi onayla. Önceki denemede dosyalarda yapılmış değişiklikler çalışma klasöründe bulunabilir.

Yeni görevlerde Codex oturum kimliği iş bitmeden yerel hesaba kaydedilir. Kesinti sonrasında **Devamı onaya al**, kaydedilmiş görev geçmişiyle devamı hazırlar. Uygulama yeniden açılsa da bu geçmiş korunur; görev kendiliğinden yeniden çalışmaz. Ağ, web, MCP ve pencere izinleri yeniden seçilir. Önceki komutların dosyalarda yapmış olabileceği değişiklikleri kontrol ederek devam etmesi Codex'e bildirilir.

Güncel hostta başlangıç mesajının altında canlı görev durumu görünür. Ajan çalışmaya başladığında, yerel onay ya da sohbet yanıtı beklediğinde ve görev bittiğinde bu satır güncellenir. Codex bir plan bildirdiyse tamamlanan adım sayısı da görünür.

Codex bir ayrıntıya ihtiyaç duyduğunda bire bir sohbete **Codex görev sorusu** gönderip üç dakika cevap bekleyebilir. Soruyu alıntılayarak yazdığın metin aynı göreve iletilir. Pencere eylemleri ve dosya paylaşımı için yerel onay akışı geçerlidir.

Codex bir dosya değişikliği bildirdiğinde **Dosya değişikliklerini incele** bölümü eklenen ve çıkarılan satırları gösterir. **Komut etkinliği** son komutları, çıktı önizlemelerini ve çıkış kodlarını gösterir. Tamamlanan görevlerin planı, komutları, dosya farkı ve token kullanımı yerel hesap verisine kaydedilir ve uygulama yeniden açıldığında görüntülenebilir. Sohbetteki **Görev durumu**, Codex bir plan bildirdiyse tamamlanan adım sayısını gösterir.

Bir görev mesajına dosya veya ekran görüntüsü ekleyebilirsin. Ajan sağ panelde ekin adını gösterir ve görev onayı verilene kadar eki indirmez. JPEG ve PNG görsel olarak, küçük UTF-8 dosyalar metin olarak Codex’e iletilir. Diğer dosyalar görev sırasında okunabilen geçici bir kopyayla sunulur.

Yeni görevlerde «oluşturduğun dosyayı sohbete ekle» diyebilirsin. Codex’in seçtiği dosyanın adı, göreli yolu ve boyutu sağ panelde **Dosya paylaşımı** altında görünür. Dosyayı ayrıca onaylarsan görev tamamlandığında alıntılı yanıta eklenir. Güncel host 32 MiB’a kadar, ek destekleyen eski host 1 MiB’a kadar dosya kabul eder. Her görev yanıtı bir ek taşır.

Windows’ta operatör görevi başlatmadan önce bu panelde bir pencere seçebilir. Pencere görüntüsü alma, tıklama, kaydırma ve metin yazma işlemleri ayrı ayrı yerel onay ister. Görüntü analizi onaylanırsa pencerenin görüntüsü Codex’e gönderilir. Pencere onayı, görüntünün sohbet eki olarak da gönderilip gönderilmeyeceğini gösterir.

Yeni görevlerde Codex çalışırken ilk pencereyi veya başka bir pencereyi seçmeni isteyebilir. **Pencere araçları** bölümünde isteğin nedenini görür, açık pencerelerden birini seçer veya isteği reddedersin. Seçim görev boyunca geçerli olur. Her görüntü ve giriş eylemi ayrıca onay ister.

Codex ayrıca Windows erişilebilirlik bilgilerinden görünür düğme adlarını, öğe türlerini ve konumlarını okumak için izin isteyebilir. Parola alanları ve düzenlenebilir alanların içeriği okunmaz. Okunabilen öğeler, uygulamanın Windows’a sunduğu erişilebilirlik bilgilerine bağlıdır.

Yeni görevlerde Codex, son onaylı okumada bulunan bir düğmeyi fareyi hareket ettirmeden çalıştırmayı isteyebilir. Onay kartında düğmenin adı gösterilir. Onay sırasında düğme değişirse önce pencerenin yeniden okunması gerekir.

Yeni görevler sağ tık, çift tık ve klavyeyle gezinmeyi de destekler. Ajan Tab ile odak değiştirebilir, Enter ile odaktaki öğeyi seçebilir ve Ctrl+F veya Ctrl+S gibi uygulama içi kısayolları kullanabilir. Bu eylemler pencere odakta kaldığı sürece uygulanır. Onay kartı gönderilecek tuşu, kısayolu veya tıklama türünü gösterir.

Ajan seçili pencere içinde imleci bir öğenin üzerine götürebilir ve bir öğeyi sürükleyip bırakabilir. Sürükleme onayı başlangıç ve bitiş noktalarını gösterir. Yol başka bir pencere tarafından kapatılırsa işlem uygulanmaz.

Görev onayında **Bu görevde web araştırmasına izin ver** seçeneğini işaretlersen ajan OpenAI’nin web arama aracını kullanabilir. Seçim yalnızca o görev için geçerlidir. Arama sorguları görev panelinde, kaynak bağlantıları sohbet yanıtında görünür.

**Bu görev için alt ajanlara iş devret** seçeneği, Codex'in yerel alt ajan araçlarını açar. Ana ajan, onaylı çalışma klasöründeki bağımsız alt işleri devredebilir ve sonuçlarını birleştirir. Aynı anda en fazla üç alt ajan çalışacak şekilde yapılandırılır. Görev panelindeki **Alt ajanlar** bölümü adları, kayıtlı model ve düşünme düzeyini ve çalışma durumunu gösterir. Bu bilgiler yerel görev raporunda korunur. Pencere işlemleri, soru sorma, dosya paylaşımı ve MCP çağrıları ana ajan üzerinden mevcut onaylarla yürütülür. Alt ajan seçimi her görev turunda yeniden yapılır; planlama ve Git incelemesinde açılmaz.

**Çalışma komutları ağ erişimi kullanabilsin** seçeneği, çalıştırma görevinde paket indirme ve ağ isteklerine izin verir. Varsayılan olarak kapalıdır. Her devam adımında yeniden seçilir ve salt okunur planlama veya kod incelemesinde uygulanmaz.

Bu seçim Codex'in yerel sandbox ağ politikasına iletilir. ChatOid, Windows görevleri için `elevated` sandbox türünü seçer. Codex'in sandbox kurulumu cihazda tamamlanmış olmalıdır; kurulum başarısızsa ChatOid daha zayıf bir sandbox'a geçmez. Windows'taki yerel testte bu sandbox'ta ağ izni kapalıyken de aynı cihazdaki HTTP sunucusuna erişilebildi. Kapalı seçim tam ağ yalıtımı garantisi vermez. İndirme testi ve bu sınırı ölçen ayrı tanılama testi `tests/acceptance.rs` içindedir.

İnsan parolası ve ajan özel anahtarı yerel cihazda kalır. Ajanın `.key` dosyası hesabın anahtarıdır. Sadece imzalı kişi kartını paylaş. Sohbet kayıtları hostta saklanır ve hostu çalıştıran kişi bu kayıtlara erişebilir. Codex sağlayıcısına gönderilen istek, ilgili sohbetin sınırlı bağlamını ve izinli dosya alıntılarını içerebilir.

## Derleme ve test

Rust 1.88 veya üstü ile yerel bir C derleyicisi gerekir. Windows için MSVC kullan. Ubuntu 24.04’te pencere ve OpenGL bağımlılıkları için `libx11-dev`, `libxcb1-dev`, `libxkbcommon-dev`, `libwayland-dev` ve `libgl1-mesa-dev` paketlerini kur.

```sh
cargo build --release --locked --bins
cargo fmt --check
cargo test --locked
cargo test --locked --test acceptance real_codex_natural_address_and_workspace_read -- --ignored
```

Son komut, oturum açılmış yerel Codex CLI ile gerçek model çağrısı yapar. Test verisi geçici bir klasörde oluşturulur. Pencereyi açmadan kimlik, HTTPS, ayrı grup onayı ve dosya incelemeli ajan yanıtını sınar.

Ayrı deneme profilleri için her iki istemci de `--data-dir KLASÖR` kabul eder. Uygulamalar bunun altında ayrı `human` ve `agent` dizinleri kullanır.

## Sürüm arşivleri

Python 3.11 veya üstüyle mevcut platformu ve kaynak dosyalarını paketle.

```sh
python tools/package.py windows
python tools/package.py linux
python tools/package.py source
python tools/package.py manifest
python tools/package.py verify
```

`windows` ve `linux` komutları, o platformda üretilen `target/release` dosyalarını kullanır. İki platformun arşivlerini aynı `dist` dizininde birleştirip SHA-256 listesini oluştur. GitHub Actions, `v0.1.0` gibi bir sürüm etiketi gönderildiğinde iki platformda derler, test eder ve doğrulanmış dağıtım çıktısını saklar.

### Komut menüsü, döngüler ve hedefler

Mesaj kutusunda `/` yazınca yalnızca `/loop` ve `/goal` seçenekleri, açıklama ve simgeleriyle açılır. Yukarı/aşağı ok ve Enter ile seçilebilir; Escape kapatır.

- `/loop 1h <iş>`: işi her saatte bir çalıştırır. `10s`, `30m`, `1h`, `1d` veya `saatlik` kullanılabilir. `/loop list` kayıtları gösterir, `/loop stop <kimlik>` durdurur. Uygulama ve sohbet hostu açıkken çalışır; yeniden açılınca kaçırılan aralıklar topluca çalıştırılmaz.
- `/goal <hedef>`: hedefe ulaşılana kadar ilerlemeyi kaydederek çalışır. `/goal status` durumunu gösterir; `/goal stop` durdurur, `/goal resume` devam ettirir. Tamamlanma kararı yapılandırılmış Codex araç sonucundan gelir. Üç ardışık ilerlemesiz denemede engellendi durumuna geçer.

Döngü ve hedefler hesaba ve sohbete bağlıdır; başka bir kişi bunları yönetemez. Operatör, üyelik ve host bağlantısı her çalıştırmada tekrar doğrulanır.
