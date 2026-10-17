package com.example.personnel;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.List;
import java.util.Map;

import com.fasterxml.jackson.databind.ObjectMapper;

import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.AfterAll;

import org.springframework.http.HttpMethod;
import org.springframework.http.HttpEntity;
import org.springframework.http.HttpHeaders;
import org.springframework.http.ResponseEntity;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.Test;
import org.springframework.test.context.TestConstructor;
import org.springframework.test.context.TestConstructor.AutowireMode;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.boot.test.context.SpringBootTest;
import org.springframework.boot.test.web.client.TestRestTemplate;
import org.springframework.test.context.DynamicPropertyRegistry;
import org.springframework.test.context.DynamicPropertySource;

import com.example.personnel.model.dto.PayrollRow;
import com.example.personnel.model.dto.Totals;
import com.example.personnel.model.entity.Company;
import com.example.personnel.model.entity.Employee;
import com.example.personnel.repository.EmployeeRepository;

/**
 * The application against a throwaway server: the acceptance gate for client
 * library changes — everything the app does runs through the library here.
 */
@TestConstructor(autowireMode = AutowireMode.ALL)
@SpringBootTest(webEnvironment = SpringBootTest.WebEnvironment.RANDOM_PORT)
class PersonnelApiIT {

    private final TestRestTemplate http;
    private final EmployeeRepository employees;
    private final com.example.personnel.repository.ReviewsRepository reviews;

    PersonnelApiIT(TestRestTemplate http, EmployeeRepository employees,
                   com.example.personnel.repository.ReviewsRepository reviews) {
        this.http = http;
        this.employees = employees;
        this.reviews = reviews;
    }

    @BeforeAll
    static void serverUp() throws Exception {
        String url = TestServer.start();
        // fresh repository for this run, pointed at the test server
        System.setProperty("graphrd.test.url", url);
    }

    @DynamicPropertySource
    static void pointing(DynamicPropertyRegistry registry) {
        registry.add("graphrd.base-url", () -> System.getProperty("graphrd.test.url"));
        registry.add("graphrd.repository", () -> "personnel-it");
    }

    @AfterAll
    static void serverDown() {
        TestServer.stop();
    }

    @Test
    @SuppressWarnings("unchecked")
    void fullApplicationTour() throws Exception {
        reviews.deleteAll();   // a raise from an earlier run must not cost anything here
        TestRestTemplate rest = http;

        // the seed ran: 5 people, 3 companies
        Map<String, Object> list = rest.getForObject("/employees?size=50", Map.class);
        assertEquals(5, ((Number) ((Map<?, ?>) list.get("page")).get("totalElements")).intValue());
        assertEquals(3, rest.getForObject("/companies", List.class).size());

        // Spring's own Page as the answer: content, totalElements, pageable
        Map<String, Object> page = rest.getForObject(
                "/employees?page=0&size=2&sort=salary,desc", Map.class);
        assertEquals(5, ((Number) ((Map<?, ?>) page.get("page")).get("totalElements")).intValue());
        assertEquals(2, ((List<?>) page.get("content")).size());
        assertEquals(3, ((Number) ((Map<?, ?>) page.get("page")).get("totalPages")).intValue());

        // filters, including a company condition that is an IRI join
        Map<String, Object> atAcme = rest.getForObject(
                "/employees?company=http://example.org/company/acme&minAge=30", Map.class);
        assertEquals(1, ((Number) ((Map<?, ?>) atAcme.get("page")).get("totalElements")).intValue());

        // case-blind search (library whereILike): 'aL' still finds Alice
        Map<String, Object> found = rest.getForObject("/employees?nameStarts=aL", Map.class);
        assertEquals(1, ((Number) ((Map<?, ?>) found.get("page")).get("totalElements")).intValue());

        // create: the IRI is minted by the library now (generated id)
        ResponseEntity<String> createdResp = rest.postForEntity("/employees",
                Map.of("name", "Grace", "email", "grace@example.org", "age", 47,
                        "salary", 99000, "company", "http://example.org/company/globex"),
                String.class);
        assertTrue(createdResp.getStatusCode().is2xxSuccessful(),
                "create failed: " + createdResp.getStatusCode() + " " + createdResp.getBody());
        Map<String, Object> created = new ObjectMapper()
                .readValue(createdResp.getBody(), Map.class);
        assertNotNull(created.get("id"), "the library minted an IRI");
        assertTrue(created.get("id").toString().startsWith("http://example.org/employee/"),
                created.get("id").toString());

        // validation: duplicate email, unknown company, oversized raise
        assertEquals(400, rest.postForEntity("/employees",
                Map.of("name", "G2", "email", "grace@example.org", "salary", 1,
                        "company", "http://example.org/company/globex"), String.class)
                .getStatusCode().value());
        assertEquals(400, rest.postForEntity("/employees",
                Map.of("name", "G3", "email", "g3@example.org", "salary", 1,
                        "company", "http://example.org/nope"), String.class)
                .getStatusCode().value());

        String id = created.get("id").toString();

        long cost0 = rest.getForObject("/reports/raise-cost", Long.class);
        // a raise updates and leaves a record; history reads it back
        // (the IRI travels as a {id} template variable: encoded here, decoded there)
        Map<String, Object> raised = rest.postForObject("/employees/raise?id={id}",
                Map.of("amount", 4000, "reason", "merit"), Map.class, id);
        assertTrue(raised.containsKey("salary"), "raise failed: " + raised);
        assertEquals(103000, ((Number) raised.get("salary")).intValue());
        List<Map<String, Object>> history = (List<Map<String, Object>>) (List<?>) rest
                .getForObject("/employees/history?id={id}", List.class, id);
        assertEquals(1, history.size());
        assertEquals(4000, ((Number) history.get(0).get("amount")).intValue());
        assertEquals(cost0 + 4000, rest.getForObject("/reports/raise-cost", Long.class).longValue(), "cost0 was " + cost0);
        org.junit.jupiter.api.Assertions.assertEquals(0, cost0, "reviews were cleaned at the start: " + cost0);

        // parameterized @Query answers: by name, and since a date
        assertTrue(rest.getForObject("/reports/raise-cost?since=2020-01-01", Long.class) >= 4000);
        assertEquals(0, rest.getForObject("/reports/raise-cost?since=2999-01-01", Long.class).longValue());
        var acme = rest.getForObject("/reports/payroll/company?name=Acme", java.util.Map.class);
        assertEquals(2L, ((Number) acme.get("heads")).longValue());
        assertEquals("Acme", acme.get("company"));

        // reports agree with each other: payroll rows sum to the totals
        PayrollRow[] rows = rest.getForObject("/reports/payroll", PayrollRow[].class);
        Totals totals = rest.getForObject("/reports/totals", Totals.class);
        assertEquals(6, totals.employees());
        long sum = 0;
        for (PayrollRow r : rows) {
            sum += r.total();
        }
        assertEquals(totals.payroll(), sum);

        // the same employee, negotiated as RDF: JSON-LD, Turtle, N-Triples
        var headers = new org.springframework.http.HttpHeaders();
        headers.set(HttpHeaders.ACCEPT, "application/ld+json");
        var ld = rest.exchange("/employees/one?id={id}", HttpMethod.GET,
                new org.springframework.http.HttpEntity<>(headers), String.class, id).getBody();
        assertTrue(ld != null && ld.contains("\"@type\":\"http://example.org/Employee\""), ld);

        headers.set(HttpHeaders.ACCEPT, "text/turtle");
        var ttl = rest.exchange("/employees/one?id={id}", HttpMethod.GET,
                new org.springframework.http.HttpEntity<>(headers), String.class, id).getBody();
        assertTrue(ttl != null && ttl.contains("ex:salary"), ttl);

        headers.set(HttpHeaders.ACCEPT, "application/n-triples");
        var nq = rest.exchange("/employees/one?id={id}", HttpMethod.GET,
                new org.springframework.http.HttpEntity<>(headers), String.class, id).getBody();
        assertTrue(nq != null && nq.contains(" .\n"), nq);

        headers.set(HttpHeaders.ACCEPT, "application/ld+json");
        var all = rest.exchange("/employees/rdf", HttpMethod.GET,
                new org.springframework.http.HttpEntity<>(headers), String.class).getBody();
        assertTrue(all != null && all.startsWith("[") && all.contains("Grace"), all.substring(0, 80));

        // delete and 404 afterwards
        assertEquals(200, rest.exchange("/employees/one?id={id}", HttpMethod.DELETE,
                null, String.class, id).getStatusCode().value());
        assertEquals(404, rest.getForEntity("/employees/one?id={id}", String.class, id).getStatusCode().value());
        assertTrue(employees.findById(id).isEmpty());
    }
}
