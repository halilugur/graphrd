package com.example.personnel.controller;

import java.util.List;

import org.springframework.data.domain.Pageable;
import org.springframework.data.web.PageableDefault;
import org.springframework.http.HttpHeaders;
import org.springframework.http.MediaType;
import org.springframework.http.ResponseEntity;
import org.springframework.web.bind.annotation.DeleteMapping;
import org.springframework.data.domain.Page;
import org.springframework.web.bind.annotation.GetMapping;
import org.springframework.web.bind.annotation.ModelAttribute;
import org.springframework.web.bind.annotation.PostMapping;
import org.springframework.web.bind.annotation.RequestBody;
import org.springframework.web.bind.annotation.RequestHeader;
import org.springframework.web.bind.annotation.RequestMapping;
import org.springframework.web.bind.annotation.RequestParam;
import org.springframework.web.bind.annotation.RestController;

import com.example.personnel.model.dto.EmployeeIn;
import com.example.personnel.model.dto.EmployeeOut;
import com.example.personnel.model.dto.HistoryEntry;
import com.example.personnel.model.dto.RaiseIn;
import com.example.personnel.model.filter.EmployeeSearch;
import com.example.personnel.model.dto.RdfRepresentation;
import com.example.personnel.service.EmployeeService;

/**
 * Employees. IRIs contain slashes, so they travel as the {@code id} query
 * parameter — no path encoding games, no server configuration. The listing
 * takes Spring's Pageable and a bound search object; and the same employee
 * is also served as RDF, the format picked by the Accept header.
 */
@RestController
@RequestMapping("/employees")
class EmployeeController {

    private final EmployeeService employees;

    EmployeeController(EmployeeService employees) {
        this.employees = employees;
    }

    /**
     * A paged, filtered listing, the Spring way end to end: filters bind from
     * the query string, the page arrives as Spring's Pageable, and the answer
     * is Spring's Page — content, totalElements, pageable and all.
     */
    @GetMapping(produces = MediaType.APPLICATION_JSON_VALUE)
    Page<EmployeeOut> list(@ModelAttribute EmployeeSearch search,
                           @PageableDefault(size = 20, sort = "name") Pageable pageable) {
        return employees.search(search, pageable);
    }

    /** One employee by IRI, as JSON. */
    @GetMapping(value = "/one", produces = MediaType.APPLICATION_JSON_VALUE)
    EmployeeOut get(@RequestParam String id) {
        return employees.get(id);
    }

    /**
     * The same employee as RDF — content negotiation decides:
     * {@code Accept: application/ld+json} (or any JSON), {@code text/turtle},
     * {@code application/n-triples}. The whole graph of the employee, ready
     * for any RDF tool.
     */
    @GetMapping(value = "/one", produces = {"application/ld+json", "text/turtle", "application/n-triples"})
    ResponseEntity<String> getRdf(@RequestParam String id,
                                  @RequestHeader(value = HttpHeaders.ACCEPT, required = false) String accept) {
        return answer(employees.oneAsRdf(id, accept));
    }

    /** The whole listing as one RDF document (Accept picks the format). */
    @GetMapping(value = "/rdf", produces = {"application/ld+json", "text/turtle", "application/n-triples"})
    ResponseEntity<String> listRdf(@RequestHeader(value = HttpHeaders.ACCEPT, required = false) String accept) {
        return answer(employees.listAsRdf(accept));
    }

    /** The only thing the controller does with an RDF answer: wrap it in HTTP. */
    private static ResponseEntity<String> answer(RdfRepresentation representation) {
        return ResponseEntity.ok()
                .contentType(MediaType.parseMediaType(representation.mediaType()))
                .body(representation.body());
    }

    /** The raise history of one employee, newest first. */
    @GetMapping("/history")
    List<HistoryEntry> history(@RequestParam String id) {
        return employees.history(id);
    }

    @PostMapping
    ResponseEntity<EmployeeOut> create(@RequestBody EmployeeIn in) {
        return ResponseEntity.status(org.springframework.http.HttpStatus.CREATED)
                .body(employees.create(in));
    }

    /** A raise updates the salary and leaves a review on record. */
    @PostMapping("/raise")
    EmployeeOut raise(@RequestParam String id, @RequestBody RaiseIn in) {
        return employees.raise(id, in.amount(), in.reason());
    }

    @DeleteMapping("/one")
    void delete(@RequestParam String id) {
        employees.delete(id);
    }
}
