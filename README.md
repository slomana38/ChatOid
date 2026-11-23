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
